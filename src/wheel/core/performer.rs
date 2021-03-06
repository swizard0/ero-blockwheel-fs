use std::mem;

use alloc_pool::bytes::{
    Bytes,
    BytesPool,
};

use crate::{
    Info,
    InterpretStats,
    proto,
    storage,
    context::Context,
    wheel::{
        lru,
        core::{
            task,
            block,
            schema,
            defrag,
            SpaceKey,
            BlockGet,
            BlockEntry,
            BlockEntryGet,
        },
    },
};

#[cfg(test)]
mod tests;

struct Inner<C> where C: Context {
    schema: schema::Schema,
    lru_cache: lru::Cache,
    blocks_pool: BytesPool,
    defrag: Option<Defrag<C::WriteBlock>>,
    bg_task: BackgroundTask<C::Interpreter>,
    tasks_queue: task::queue::Queue<C>,
    done_task: DoneTask,
    interpret_stats: InterpretStats,
}

struct Defrag<C> {
    queues: defrag::Queues<C>,
    in_progress_tasks_count: usize,
    in_progress_tasks_limit: usize,
}

enum DoneTask {
    None,
    ReadBlock {
        block_id: block::Id,
        block_bytes: Bytes,
        block_crc: u64,
    },
    DeleteBlockRegular {
        block_id: block::Id,
        block_entry: BlockEntry,
        freed_space_key: SpaceKey,
    },
    DeleteBlockDefrag {
        block_id: block::Id,
        block_bytes: Bytes,
        block_crc: u64,
        freed_space_key: SpaceKey,
    },
}

pub struct Performer<C> where C: Context {
    inner: Inner<C>,
}

pub enum Op<C> where C: Context {
    Idle(Performer<C>),
    Query(QueryOp<C>),
    Event(Event<C>),
}

pub enum QueryOp<C> where C: Context {
    PollRequestAndInterpreter(PollRequestAndInterpreter<C>),
    PollRequest(PollRequest<C>),
    InterpretTask(InterpretTask<C>),
    MakeIterBlocksStream(MakeIterBlocksStream<C>),
}

pub struct MakeIterBlocksStream<C> where C: Context {
    pub blocks_total_count: usize,
    pub blocks_total_size: usize,
    pub iter_blocks_context: C::IterBlocks,
    pub next: MakeIterBlocksStreamNext<C>,
}

pub struct PollRequestAndInterpreter<C> where C: Context {
    pub interpreter_context: C::Interpreter,
    pub next: PollRequestAndInterpreterNext<C>,
}

pub struct PollRequest<C> where C: Context {
    pub next: PollRequestNext<C>,
}

pub struct Event<C> where C: Context {
    pub op: EventOp<C>,
    pub performer: Performer<C>,
}

pub enum EventOp<C> where C: Context {
    Info(TaskDoneOp<C::Info, InfoOp>),
    Flush(TaskDoneOp<C::Flush, FlushOp>),
    WriteBlock(TaskDoneOp<C::WriteBlock, WriteBlockOp>),
    ReadBlock(TaskDoneOp<C::ReadBlock, ReadBlockOp>),
    DeleteBlock(TaskDoneOp<C::DeleteBlock, DeleteBlockOp>),
    IterBlocksItem(IterBlocksItemOp<C::IterBlocksStream>),
    IterBlocksFinish(IterBlocksFinishOp<C::IterBlocksStream>),
}

pub struct TaskDoneOp<C, O> {
    pub context: C,
    pub op: O,
}

pub enum InfoOp {
    Success { info: Info, },
}

pub enum FlushOp {
    Flushed,
}

pub enum WriteBlockOp {
    NoSpaceLeft,
    Done { block_id: block::Id, },
}

pub enum ReadBlockOp {
    NotFound,
    Done { block_bytes: Bytes, },
}

pub enum DeleteBlockOp {
    NotFound,
    Done { block_id: block::Id, },
}

pub struct IterBlocksItemOp<C> {
    pub block_id: block::Id,
    pub block_bytes: Bytes,
    pub iter_blocks_state: IterBlocksState<C>,
}

#[derive(Debug)]
pub struct IterBlocksState<C> {
    pub iter_blocks_stream_context: C,
    pub iter_blocks_cursor: IterBlocksCursor,
}

#[derive(Debug)]
pub struct IterBlocksCursor {
    block_id: block::Id,
}

pub struct IterBlocksFinishOp<C> {
    pub iter_blocks_stream_context: C,
}

pub struct InterpretTask<C> where C: Context {
    pub offset: u64,
    pub task: task::Task<C>,
    pub next: InterpretTaskNext<C>,
}

pub struct InterpretTaskNext<C> where C: Context {
    inner: Inner<C>,
}

pub struct PollRequestAndInterpreterNext<C> where C: Context {
    inner: Inner<C>,
}

pub struct PollRequestNext<C> where C: Context {
    inner: Inner<C>,
}

pub struct MakeIterBlocksStreamNext<C> where C: Context {
    inner: Inner<C>,
}

pub struct DefragConfig<C> {
    queues: defrag::Queues<C>,
    in_progress_tasks_limit: usize,
}

impl<C> DefragConfig<C> {
    pub fn new(in_progress_tasks_limit: usize) -> DefragConfig<C> {
        DefragConfig {
            queues: defrag::Queues::new(),
            in_progress_tasks_limit,
        }
    }
}

#[derive(Debug)]
pub enum BuilderError {
    StorageLayoutCalculate(storage::LayoutError),
}

pub struct PerformerBuilderInit<C> where C: Context {
    lru_cache: lru::Cache,
    blocks_pool: BytesPool,
    defrag: Option<Defrag<C::WriteBlock>>,
    storage_layout: storage::Layout,
    work_block: Vec<u8>,
}

impl<C> PerformerBuilderInit<C> where C: Context {
    pub fn new(
        lru_cache: lru::Cache,
        blocks_pool: BytesPool,
        defrag_queues: Option<DefragConfig<C::WriteBlock>>,
        work_block_size_bytes: usize,
    )
        -> Result<PerformerBuilderInit<C>, BuilderError>
    {
        let mut work_block = Vec::with_capacity(work_block_size_bytes);
        let storage_layout = storage::Layout::calculate(&mut work_block)
            .map_err(BuilderError::StorageLayoutCalculate)?;

        Ok(PerformerBuilderInit {
            lru_cache,
            blocks_pool,
            defrag: defrag_queues
                .map(|config| Defrag {
                    queues: config.queues,
                    in_progress_tasks_count: 0,
                    in_progress_tasks_limit: config.in_progress_tasks_limit,
                }),
            storage_layout,
            work_block,
        })
    }

    pub fn storage_layout(&self) -> &storage::Layout {
        &self.storage_layout
    }

    pub fn work_block_cleared(&mut self) -> &mut Vec<u8> {
        self.work_block.clear();
        self.work_block()
    }

    pub fn work_block(&mut self) -> &mut Vec<u8> {
        &mut self.work_block
    }

    pub fn start_fill(self) -> (PerformerBuilder<C>, Vec<u8>) {
        let schema_builder = schema::Builder::new(self.storage_layout);
        (
            PerformerBuilder {
                schema_builder,
                lru_cache: self.lru_cache,
                blocks_pool: self.blocks_pool,
                defrag: self.defrag,
            },
            self.work_block,
        )
    }
}

pub struct PerformerBuilder<C> where C: Context {
    schema_builder: schema::Builder,
    lru_cache: lru::Cache,
    blocks_pool: BytesPool,
    defrag: Option<Defrag<C::WriteBlock>>,
}

impl<C> PerformerBuilder<C> where C: Context {
    pub fn push_block(&mut self, offset: u64, block_header: storage::BlockHeader) {
        let defrag_op = self.schema_builder.push_block(offset, block_header);
        if let Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }) = self.defrag.as_mut() {
            match defrag_op {
                schema::DefragOp::Queue { defrag_gaps, moving_block_id, } =>
                    tasks.push(defrag_gaps, moving_block_id),
                schema::DefragOp::None =>
                    (),
            }
        }
    }

    pub fn storage_layout(&self) -> &storage::Layout {
        self.schema_builder.storage_layout()
    }

    pub fn finish(mut self, size_bytes_total: usize) -> Performer<C> {
        let (defrag_op, schema) = self.schema_builder.finish(size_bytes_total);
        if let Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }) = self.defrag.as_mut() {
            match defrag_op {
                schema::DefragOp::Queue { defrag_gaps, moving_block_id, } =>
                    tasks.push(defrag_gaps, moving_block_id),
                schema::DefragOp::None =>
                    (),
            }
        }

        Performer {
            inner: Inner::new(
                schema,
                self.lru_cache,
                self.blocks_pool,
                self.defrag,
            ),
        }
    }
}

impl<C> Performer<C> where C: Context {
    pub fn next(self) -> Op<C> {
        self.inner.incoming_poke()
    }

    #[cfg(test)]
    pub fn decompose(self) -> schema::Schema {
        self.inner.schema
    }
}

impl<C> PollRequestAndInterpreterNext<C> where C: Context {
    pub fn incoming_request(mut self, request: proto::Request<C>, interpreter_context: C::Interpreter) -> Op<C> {
        self.inner.bg_task.state = match self.inner.bg_task.state {
            BackgroundTaskState::Await { block_id, } =>
                BackgroundTaskState::InProgress { block_id, interpreter_context, },
            BackgroundTaskState::Idle | BackgroundTaskState::InProgress { .. } =>
                unreachable!(),
        };
        self.inner.incoming_request(request)
    }

    #[cfg(test)]
    pub fn incoming_task_done(self, task_done: task::Done<C>) -> Op<C> {
        self.inner.incoming_interpreter(task_done)
    }

    pub fn incoming_task_done_stats(mut self, task_done: task::Done<C>, stats: InterpretStats) -> Op<C> {
        self.inner.interpret_stats = stats;
        self.inner.incoming_interpreter(task_done)
    }

    pub fn incoming_iter_blocks(
        mut self,
        iter_blocks_state: IterBlocksState<C::IterBlocksStream>,
        interpreter_context: C::Interpreter,
    )
        -> Op<C>
    {
        self.inner.bg_task.state = match self.inner.bg_task.state {
            BackgroundTaskState::Await { block_id, } =>
                BackgroundTaskState::InProgress { block_id, interpreter_context, },
            BackgroundTaskState::Idle | BackgroundTaskState::InProgress { .. } =>
                unreachable!(),
        };
        self.inner.iter_blocks_stream_next(
            iter_blocks_state.iter_blocks_cursor.block_id,
            iter_blocks_state.iter_blocks_stream_context,
        )
    }
}

impl<C> PollRequestNext<C> where C: Context {
    pub fn incoming_request(self, request: proto::Request<C>) -> Op<C> {
        self.inner.incoming_request(request)
    }

    pub fn incoming_iter_blocks(self, iter_blocks_state: IterBlocksState<C::IterBlocksStream>) -> Op<C> {
        self.inner.iter_blocks_stream_next(
            iter_blocks_state.iter_blocks_cursor.block_id,
            iter_blocks_state.iter_blocks_stream_context,
        )
    }
}

impl<C> InterpretTaskNext<C> where C: Context {
    pub fn task_accepted(mut self, interpreter_context: C::Interpreter) -> Performer<C> {
        self.inner.bg_task.state = match self.inner.bg_task.state {
            BackgroundTaskState::Await { block_id, } =>
                BackgroundTaskState::InProgress { block_id, interpreter_context, },
            BackgroundTaskState::Idle | BackgroundTaskState::InProgress { .. } =>
                unreachable!(),
        };
        Performer { inner: self.inner, }
    }
}

impl<C> MakeIterBlocksStreamNext<C> where C: Context {
    pub fn stream_ready(self, iter_blocks_stream_context: C::IterBlocksStream) -> Op<C> {
        self.inner.iter_blocks_stream_ready(iter_blocks_stream_context)
    }
}


struct BackgroundTask<C> {
    current_offset: u64,
    state: BackgroundTaskState<C>,
}

enum BackgroundTaskState<C> {
    Idle,
    InProgress {
        block_id: block::Id,
        interpreter_context: C,
    },
    Await {
        block_id: block::Id,
    }
}

impl<C> Inner<C> where C: Context {
    fn new(
        schema: schema::Schema,
        lru_cache: lru::Cache,
        blocks_pool: BytesPool,
        defrag: Option<Defrag<C::WriteBlock>>,
    )
        -> Inner<C>
    {
        Inner {
            schema,
            lru_cache,
            blocks_pool,
            tasks_queue: task::queue::Queue::new(),
            defrag,
            bg_task: BackgroundTask {
                current_offset: 0,
                state: BackgroundTaskState::Idle,
            },
            done_task: DoneTask::None,
            interpret_stats: InterpretStats::default(),
        }
    }

    fn incoming_poke(mut self) -> Op<C> {
        match mem::replace(&mut self.done_task, DoneTask::None) {
            DoneTask::None =>
                (),
            DoneTask::ReadBlock { block_id, block_bytes, block_crc, } => {
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                assert!(lens.pop_write_task(self.schema.block_get()).is_none());
                if let Some(read_block) = lens.pop_read_task(self.schema.block_get()) {
                    self.done_task = DoneTask::ReadBlock {
                        block_id: block_id.clone(),
                        block_bytes: block_bytes.clone(),
                        block_crc,
                    };
                    return self.proceed_read_block_task_done(block_id, block_bytes, block_crc, read_block.context);
                }
                lens.enqueue(self.schema.block_get());
            },
            DoneTask::DeleteBlockRegular { block_id, mut block_entry, freed_space_key, } => {
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                let mut block_get = BlockEntryGet::new(&mut block_entry);
                while let Some(write_block) = lens.pop_write_task(&mut block_get) {
                    match write_block.context {
                        task::WriteBlockContext::External(..) =>
                            unreachable!(),
                        task::WriteBlockContext::Defrag { .. } => {
                            // cancel defrag write task
                            cancel_defrag_task(self.defrag.as_mut().unwrap());
                        },
                    }
                }
                while let Some(read_block) = lens.pop_read_task(&mut block_get) {
                    match read_block.context {
                        task::ReadBlockContext::External(context) => {
                            self.done_task = DoneTask::DeleteBlockRegular {
                                block_id: block_id.clone(),
                                block_entry,
                                freed_space_key,
                            };
                            return Op::Event(Event {
                                op: EventOp::ReadBlock(TaskDoneOp {
                                    context,
                                    op: ReadBlockOp::NotFound,
                                }),
                                performer: Performer { inner: self, },
                            });
                        },
                        task::ReadBlockContext::Defrag { .. } => {
                            // cancel defrag read task
                            cancel_defrag_task(self.defrag.as_mut().unwrap());
                        },
                        task::ReadBlockContext::IterBlocks { iter_blocks_stream_context, next_block_id, } => {
                            // skip this block, proceed with the next one
                            return self.iter_blocks_stream_next(next_block_id, iter_blocks_stream_context);
                        },
                    }
                }
                while let Some(delete_block) = lens.pop_delete_task(&mut block_get) {
                    match delete_block.context {
                        task::DeleteBlockContext::External(context) => {
                            self.done_task = DoneTask::DeleteBlockRegular {
                                block_id: block_id.clone(),
                                block_entry,
                                freed_space_key,
                            };
                            return Op::Event(Event {
                                op: EventOp::DeleteBlock(TaskDoneOp {
                                    context,
                                    op: DeleteBlockOp::NotFound,
                                }),
                                performer: Performer { inner: self, },
                            });
                        },
                        task::DeleteBlockContext::Defrag { .. } => {
                            // cancel defrag delete task
                            cancel_defrag_task(self.defrag.as_mut().unwrap());
                        },
                    }
                }
                self.flush_defrag_pending_queue(Some(freed_space_key));
            },
            DoneTask::DeleteBlockDefrag { block_id, block_bytes, block_crc, freed_space_key, } => {
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                while let Some(read_block) = lens.pop_read_task(self.schema.block_get()) {
                    self.done_task = DoneTask::DeleteBlockDefrag {
                        block_id: block_id.clone(),
                        block_bytes: block_bytes.clone(),
                        block_crc,
                        freed_space_key,
                    };
                    return self.proceed_read_block_task_done(block_id, block_bytes, block_crc, read_block.context)
                }
                lens.enqueue(self.schema.block_get());
                self.flush_defrag_pending_queue(Some(freed_space_key));
            },
        }

        if let Some(defrag) = self.defrag.as_mut() {
            loop {
                if defrag.in_progress_tasks_count >= defrag.in_progress_tasks_limit {
                    break;
                }
                if let Some((defrag_gaps, moving_block_id)) = defrag.queues.tasks.pop(self.schema.block_get()) {
                    let mut block_get = self.schema.block_get();
                    let block_entry = block_get.by_id(&moving_block_id).unwrap();
                    let block_bytes = self.blocks_pool.lend();
                    let mut lens = self.tasks_queue.focus_block_id(block_entry.header.block_id.clone());
                    lens.push_task(
                        task::Task {
                            block_id: block_entry.header.block_id.clone(),
                            kind: task::TaskKind::ReadBlock(task::ReadBlock {
                                block_header: block_entry.header.clone(),
                                block_bytes,
                                context: task::ReadBlockContext::Defrag { defrag_gaps, },
                            }),
                        },
                        self.schema.block_get(),
                    );
                    lens.enqueue(self.schema.block_get());
                    defrag.in_progress_tasks_count += 1;
                } else {
                    break;
                }
            }
        }

        if self.tasks_queue.is_empty_tasks() && self.defrag.as_ref().map_or(true, |defrag| defrag.in_progress_tasks_count == 0) {
            if let Some(task::Flush { context, }) = self.tasks_queue.pop_flush() {
                return Op::Event(Event {
                    op: EventOp::Flush(TaskDoneOp { context, op: FlushOp::Flushed, }),
                    performer: Performer { inner: self, },
                });
            }
        }

        match mem::replace(&mut self.bg_task.state, BackgroundTaskState::Idle) {
            BackgroundTaskState::Idle =>
                self.maybe_run_background_task(),
            BackgroundTaskState::InProgress { block_id, interpreter_context, } => {
                self.bg_task.state = BackgroundTaskState::Await { block_id, };
                Op::Query(QueryOp::PollRequestAndInterpreter(PollRequestAndInterpreter {
                    interpreter_context,
                    next: PollRequestAndInterpreterNext {
                        inner: self,
                    },
                }))
            },
            BackgroundTaskState::Await { .. } =>
                unreachable!(),
        }
    }

    fn incoming_request(self, incoming: proto::Request<C>) -> Op<C> {
        match incoming {
            proto::Request::Info(request_info) =>
                self.incoming_request_info(request_info),
            proto::Request::Flush(request_flush) =>
                self.incoming_request_flush(request_flush),
            proto::Request::WriteBlock(request_write_block) =>
                self.incoming_request_write_block(request_write_block),
            proto::Request::ReadBlock(request_read_block) =>
                self.incoming_request_read_block(request_read_block),
            proto::Request::DeleteBlock(request_delete_block) =>
                self.incoming_request_delete_block(request_delete_block),
            proto::Request::IterBlocks(request_iter_blocks) =>
                self.incoming_request_iter_blocks(request_iter_blocks),
        }
    }

    fn incoming_request_info(self, proto::RequestInfo { context, }: proto::RequestInfo<C::Info>) -> Op<C> {
        let mut info = self.schema.info();
        info.interpret_stats = self.interpret_stats;
        if let Some(defrag) = self.defrag.as_ref() {
            info.defrag_write_pending_bytes = defrag.queues.pending.pending_bytes();
            assert!(
                info.bytes_free >= info.defrag_write_pending_bytes,
                "assertion failed: info.bytes_free = {} >= info.defrag_write_pending_bytes = {}",
                info.bytes_free,
                info.defrag_write_pending_bytes,
            );
            info.bytes_free -= info.defrag_write_pending_bytes;
        }

        Op::Event(Event {
            op: EventOp::Info(TaskDoneOp { context, op: InfoOp::Success { info, }, }),
            performer: Performer { inner: self, },
        })
    }

    fn incoming_request_flush(mut self, proto::RequestFlush { context, }: proto::RequestFlush<C::Flush>) -> Op<C> {
        self.tasks_queue.push_flush(task::Flush { context, });
        Op::Idle(Performer { inner: self, })
    }

    fn incoming_request_write_block(mut self, request_write_block: proto::RequestWriteBlock<C::WriteBlock>) -> Op<C> {
        let defrag_pending_bytes = self.defrag
            .as_ref()
            .map(|defrag| defrag.queues.pending.pending_bytes());
        match self.schema.process_write_block_request(&request_write_block.block_bytes, defrag_pending_bytes) {

            schema::WriteBlockOp::Perform(write_block_perform) => {
                incoming_request_write_block_perform(
                    &mut self.tasks_queue,
                    self.defrag.as_mut(),
                    request_write_block,
                    write_block_perform,
                    self.schema.block_get(),
                );
                Op::Idle(Performer { inner: self, })
            },

            schema::WriteBlockOp::QueuePendingDefrag { space_required, } => {
                log::debug!(
                    "cannot directly allocate {} ({}) bytes in process_write_block_request: moving to pending defrag queue",
                    request_write_block.block_bytes.len(),
                    space_required,
                );
                if let Some(Defrag { queues: defrag::Queues { pending, .. }, .. }) = self.defrag.as_mut() {
                    pending.push(request_write_block, space_required);
                }
                Op::Idle(Performer { inner: self, })
            },

            schema::WriteBlockOp::ReplyNoSpaceLeft =>
               Op::Event(Event {
                    op: EventOp::WriteBlock(TaskDoneOp {
                        context: request_write_block.context,
                        op: WriteBlockOp::NoSpaceLeft,
                    }),
                    performer: Performer { inner: self, },
                }),

        }
    }

    fn incoming_request_read_block(mut self, request_read_block: proto::RequestReadBlock<C::ReadBlock>) -> Op<C> {
        match self.schema.process_read_block_request(&request_read_block.block_id) {

            schema::ReadBlockOp::Perform(schema::ReadBlockPerform { block_header, }) =>
                if let Some(block_bytes) = self.lru_cache.get(&request_read_block.block_id) {
                    Op::Event(Event {
                        op: EventOp::ReadBlock(TaskDoneOp {
                            context: request_read_block.context,
                            op: ReadBlockOp::Done {
                                block_bytes: block_bytes.clone(),
                            },
                        }),
                        performer: Performer { inner: self, },
                    })
                } else {
                    let block_bytes = self.blocks_pool.lend();
                    let mut lens = self.tasks_queue.focus_block_id(request_read_block.block_id.clone());
                    lens.push_task(
                        task::Task {
                            block_id: request_read_block.block_id,
                            kind: task::TaskKind::ReadBlock(task::ReadBlock {
                                block_header: block_header.clone(),
                                block_bytes,
                                context: task::ReadBlockContext::External(
                                    request_read_block.context,
                                ),
                            }),
                        },
                        self.schema.block_get(),
                    );
                    lens.enqueue(self.schema.block_get());
                    Op::Idle(Performer { inner: self, })
                },

            schema::ReadBlockOp::NotFound =>
                Op::Event(Event {
                    op: EventOp::ReadBlock(TaskDoneOp {
                        context: request_read_block.context,
                        op: ReadBlockOp::NotFound,
                    }),
                    performer: Performer { inner: self, },
                }),

        }
    }

    fn incoming_request_delete_block(mut self, request_delete_block: proto::RequestDeleteBlock<C::DeleteBlock>) -> Op<C> {
        match self.schema.process_delete_block_request(&request_delete_block.block_id) {

            schema::DeleteBlockOp::Perform(schema::DeleteBlockPerform) => {
                let mut lens = self.tasks_queue.focus_block_id(request_delete_block.block_id.clone());
                lens.push_task(
                    task::Task {
                        block_id: request_delete_block.block_id,
                        kind: task::TaskKind::DeleteBlock(task::DeleteBlock {
                            context: task::DeleteBlockContext::External(
                                request_delete_block.context,
                            ),
                        }),
                    },
                    self.schema.block_get(),
                );
                lens.enqueue(self.schema.block_get());
                Op::Idle(Performer { inner: self, })
            },

            schema::DeleteBlockOp::NotFound =>
                Op::Event(Event {
                    op: EventOp::DeleteBlock(TaskDoneOp {
                        context: request_delete_block.context,
                        op: DeleteBlockOp::NotFound,
                    }),
                    performer: Performer { inner: self, },
                }),

        }
    }

    fn incoming_request_iter_blocks(self, request_iter_blocks: proto::RequestIterBlocks<C::IterBlocks>) -> Op<C> {
        let info = self.schema.info();
        Op::Query(QueryOp::MakeIterBlocksStream(MakeIterBlocksStream {
            blocks_total_count: info.blocks_count,
            blocks_total_size: info.data_bytes_used,
            iter_blocks_context: request_iter_blocks.context,
            next: MakeIterBlocksStreamNext {
                inner: self,
            },
        }))
    }

    fn incoming_interpreter(mut self, incoming: task::Done<C>) -> Op<C> {
        match incoming {

            task::Done { current_offset, task: task::TaskDone { block_id, kind: task::TaskDoneKind::WriteBlock(write_block), }, } => {
                self.bg_task = BackgroundTask { current_offset, state: BackgroundTaskState::Idle, };
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                lens.finish(self.schema.block_get());
                lens.enqueue(self.schema.block_get());
                match write_block.context {
                    task::WriteBlockContext::External(context) =>
                        Op::Event(Event {
                            op: EventOp::WriteBlock(TaskDoneOp {
                                context,
                                op: WriteBlockOp::Done { block_id, },
                            }),
                            performer: Performer { inner: self, },
                        }),
                    task::WriteBlockContext::Defrag => {
                        let defrag = self.defrag.as_mut().unwrap();
                        assert!(defrag.in_progress_tasks_count > 0);
                        defrag.in_progress_tasks_count -= 1;
                        Op::Idle(Performer { inner: self, })
                    },
                }
            },

            task::Done { current_offset, task: task::TaskDone { block_id, kind: task::TaskDoneKind::ReadBlock(read_block), }, } => {
                self.bg_task = BackgroundTask { current_offset, state: BackgroundTaskState::Idle, };
                self.tasks_queue.focus_block_id(block_id.clone())
                    .finish(self.schema.block_get());
                self.lru_cache.insert(block_id.clone(), read_block.block_bytes.clone());
                self.done_task = DoneTask::ReadBlock {
                    block_id: block_id.clone(),
                    block_bytes: read_block.block_bytes.clone(),
                    block_crc: read_block.block_crc,
                };
                self.proceed_read_block_task_done(block_id, read_block.block_bytes, read_block.block_crc, read_block.context)
            },

            task::Done { current_offset, task: task::TaskDone { block_id, kind: task::TaskDoneKind::DeleteBlock(delete_block), }, } => {
                self.bg_task = BackgroundTask { current_offset, state: BackgroundTaskState::Idle, };
                self.tasks_queue.focus_block_id(block_id.clone())
                    .finish(self.schema.block_get());
                match delete_block.context {
                    task::DeleteBlockContext::External(context) => {
                        self.lru_cache.invalidate(&block_id);
                        match self.schema.process_delete_block_task_done(block_id.clone()) {
                            schema::DeleteBlockTaskDoneOp::Perform(schema::DeleteBlockTaskDonePerform {
                                defrag_op,
                                block_entry,
                                freed_space_key,
                            }) => {
                                if let Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }) = self.defrag.as_mut() {
                                    match defrag_op {
                                        schema::DefragOp::Queue { defrag_gaps, moving_block_id, } =>
                                            tasks.push(defrag_gaps, moving_block_id),
                                        schema::DefragOp::None =>
                                            (),
                                    }
                                }
                                self.done_task = DoneTask::DeleteBlockRegular {
                                    block_id: block_id.clone(),
                                    block_entry,
                                    freed_space_key,
                                };
                                Op::Event(Event {
                                    op: EventOp::DeleteBlock(TaskDoneOp { context, op: DeleteBlockOp::Done { block_id, }, }),
                                    performer: Performer { inner: self, },
                                })
                            },
                        }
                    },
                    task::DeleteBlockContext::Defrag { block_bytes, block_crc, .. } =>
                        match self.schema.process_delete_block_task_done_defrag(block_id.clone()) {
                            schema::DeleteBlockTaskDoneDefragOp::Perform(task_op) => {
                                if let Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }) = self.defrag.as_mut() {
                                    match task_op.defrag_op {
                                        schema::DefragOp::Queue { defrag_gaps, moving_block_id, } =>
                                            tasks.push(defrag_gaps, moving_block_id),
                                        schema::DefragOp::None =>
                                            (),
                                    }
                                }
                                self.tasks_queue.focus_block_id(block_id.clone())
                                    .push_task(
                                        task::Task {
                                            block_id: block_id.clone(),
                                            kind: task::TaskKind::WriteBlock(task::WriteBlock {
                                                block_bytes: block_bytes.clone(),
                                                block_crc: Some(block_crc),
                                                context: task::WriteBlockContext::Defrag,
                                            }),
                                        },
                                        self.schema.block_get(),
                                    );
                                self.done_task = DoneTask::DeleteBlockDefrag {
                                    block_id,
                                    block_bytes,
                                    block_crc,
                                    freed_space_key: task_op.freed_space_key,
                                };
                                Op::Idle(Performer { inner: self, })
                            },
                        },
                }
            },

        }
    }

    fn iter_blocks_stream_ready(self, iter_blocks_stream_context: C::IterBlocksStream) -> Op<C> {
        self.iter_blocks_stream_next(block::Id::init(), iter_blocks_stream_context)
    }

    fn iter_blocks_stream_next(mut self, block_id_from: block::Id, iter_blocks_stream_context: C::IterBlocksStream) -> Op<C> {
        match self.schema.next_block_id_from(block_id_from) {
            None =>
                Op::Event(Event {
                    op: EventOp::IterBlocksFinish(IterBlocksFinishOp {
                        iter_blocks_stream_context,
                    }),
                    performer: Performer { inner: self, },
                }),

            Some(block_id) =>
                match self.schema.process_read_block_request(&block_id) {

                    schema::ReadBlockOp::Perform(schema::ReadBlockPerform { block_header, }) =>
                        if let Some(block_bytes) = self.lru_cache.get(&block_id) {
                            Op::Event(Event {
                                op: EventOp::IterBlocksItem(IterBlocksItemOp {
                                    block_id: block_id.clone(),
                                    block_bytes: block_bytes.clone(),
                                    iter_blocks_state: IterBlocksState {
                                        iter_blocks_stream_context,
                                        iter_blocks_cursor: IterBlocksCursor {
                                            block_id: block_id.next(),
                                        },
                                    },
                                }),
                                performer: Performer { inner: self, },
                            })
                        } else {
                            let block_bytes = self.blocks_pool.lend();
                            let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                            lens.push_task(
                                task::Task {
                                    block_id: block_id.clone(),
                                    kind: task::TaskKind::ReadBlock(task::ReadBlock {
                                        block_header: block_header.clone(),
                                        block_bytes,
                                        context: task::ReadBlockContext::IterBlocks {
                                            iter_blocks_stream_context,
                                            next_block_id: block_id.next(),
                                        },
                                    }),
                                },
                                self.schema.block_get(),
                            );
                            lens.enqueue(self.schema.block_get());
                            Op::Idle(Performer { inner: self, })
                        },

                    schema::ReadBlockOp::NotFound =>
                        unreachable!(),

                },
        }
    }

    fn proceed_read_block_task_done(
        mut self,
        block_id: block::Id,
        block_bytes: Bytes,
        block_crc: u64,
        task_context: task::ReadBlockContext<C>,
    ) -> Op<C> {
        match self.schema.process_read_block_task_done(&block_id) {
            schema::ReadBlockTaskDoneOp::Perform(schema::ReadBlockTaskDonePerform) =>
                match task_context {
                    task::ReadBlockContext::External(context) =>
                        Op::Event(Event {
                            op: EventOp::ReadBlock(TaskDoneOp {
                                context,
                                op: ReadBlockOp::Done { block_bytes, },
                            }),
                            performer: Performer { inner: self, },
                        }),
                    task::ReadBlockContext::Defrag { defrag_gaps, } => {
                        let mut block_get = self.schema.block_get();
                        let block_entry = block_get.by_id(&block_id).unwrap();
                        let mut block_entry_get = BlockEntryGet::new(block_entry);
                        if defrag_gaps.is_still_relevant(&block_id, &mut block_entry_get) {
                            self.tasks_queue.focus_block_id(block_id.clone())
                                .push_task(
                                    task::Task {
                                        block_id: block_id.clone(),
                                        kind: task::TaskKind::DeleteBlock(task::DeleteBlock {
                                            context: task::DeleteBlockContext::Defrag {
                                                defrag_gaps,
                                                block_bytes,
                                                block_crc,
                                            },
                                        }),
                                    },
                                    &mut block_entry_get,
                                );
                        } else {
                            cancel_defrag_task(self.defrag.as_mut().unwrap());
                        }
                        Op::Idle(Performer { inner: self, })
                    },
                    task::ReadBlockContext::IterBlocks { iter_blocks_stream_context, next_block_id, } =>
                        Op::Event(Event {
                            op: EventOp::IterBlocksItem(IterBlocksItemOp {
                                block_id: block_id.clone(),
                                block_bytes: block_bytes,
                                iter_blocks_state: IterBlocksState {
                                    iter_blocks_stream_context,
                                    iter_blocks_cursor: IterBlocksCursor {
                                        block_id: next_block_id,
                                    },
                                },
                            }),
                            performer: Performer { inner: self, },
                        }),
                },
        }
    }

    fn flush_defrag_pending_queue(&mut self, mut maybe_space_key: Option<SpaceKey>) {
        if let Some(defrag) = self.defrag.as_mut() {
            loop {
                let space_key = if let Some(space_key) = maybe_space_key {
                    space_key
                } else {
                    break;
                };
                let request_write_block = if let Some(request_write_block) = defrag.queues.pending.pop_at_most(space_key.space_available()) {
                    request_write_block
                } else {
                    break;
                };
                match self.schema.process_write_block_request(&request_write_block.block_bytes, Some(defrag.queues.pending.pending_bytes())) {
                    schema::WriteBlockOp::Perform(write_block_perform) => {
                        maybe_space_key = write_block_perform.right_space_key;
                        incoming_request_write_block_perform(
                            &mut self.tasks_queue,
                            Some(defrag),
                            request_write_block,
                            write_block_perform,
                            self.schema.block_get(),
                        );
                    },
                    schema::WriteBlockOp::QueuePendingDefrag { space_required, } => {
                        defrag.queues.pending.push(request_write_block, space_required);
                        break;
                    },
                    schema::WriteBlockOp::ReplyNoSpaceLeft =>
                        unreachable!(),
                }
            }
        }
    }

    fn maybe_run_background_task(mut self) -> Op<C> {
        loop {
            if let Some((offset, mut lens)) = self.tasks_queue.next_trigger(self.bg_task.current_offset, self.schema.block_get()) {
                let task_kind = match lens.pop_task(self.schema.block_get()) {
                    Some(task_kind) => task_kind,
                    None => panic!("empty task queue unexpected for block {:?} @ {}", lens.block_id(), offset),
                };
                match &task_kind {
                    task::TaskKind::WriteBlock(..) =>
                        (),
                    task::TaskKind::ReadBlock(..) =>
                        (),
                    task::TaskKind::DeleteBlock(task::DeleteBlock { context: task::DeleteBlockContext::Defrag { defrag_gaps, .. }, }) =>
                        if !defrag_gaps.is_still_relevant(lens.block_id(), self.schema.block_get()) {
                            cancel_defrag_task(self.defrag.as_mut().unwrap());
                            lens.finish(self.schema.block_get());
                            lens.enqueue(self.schema.block_get());
                            continue;
                        },
                    task::TaskKind::DeleteBlock(task::DeleteBlock { context: task::DeleteBlockContext::External(..), }) =>
                        (),
                }

                self.bg_task.state = BackgroundTaskState::Await {
                    block_id: lens.block_id().clone(),
                };
                return Op::Query(QueryOp::InterpretTask(InterpretTask {
                    offset,
                    task: task::Task {
                        block_id: lens.block_id().clone(),
                        kind: task_kind,
                    },
                    next: InterpretTaskNext {
                        inner: self,
                    },
                }));
            } else {
                return Op::Query(QueryOp::PollRequest(PollRequest {
                    next: PollRequestNext {
                        inner: self,
                    },
                }));
            }
        }
    }
}

fn incoming_request_write_block_perform<C, B>(
    tasks_queue: &mut task::queue::Queue<C>,
    mut defrag: Option<&mut Defrag<C::WriteBlock>>,
    request_write_block: proto::RequestWriteBlock<C::WriteBlock>,
    write_block_perform: schema::WriteBlockPerform,
    mut block_get: B,
)
where C: Context,
      B: BlockGet,
{
    let schema::WriteBlockPerform { defrag_op, task_op, .. } = write_block_perform;
    if let Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }) = defrag.as_mut() {
        match defrag_op {
            schema::DefragOp::Queue { defrag_gaps, moving_block_id, } =>
                tasks.push(defrag_gaps, moving_block_id),
            schema::DefragOp::None =>
                (),
        }
    }
    let mut lens = tasks_queue.focus_block_id(task_op.block_id.clone());
    lens.push_task(
        task::Task {
            block_id: task_op.block_id,
            kind: task::TaskKind::WriteBlock(task::WriteBlock {
                block_bytes: request_write_block.block_bytes,
                block_crc: request_write_block.block_crc,
                context: task::WriteBlockContext::External(
                    request_write_block.context,
                ),
            }),
        },
        &mut block_get,
    );
    lens.enqueue(block_get);
}

fn cancel_defrag_task<C>(defrag: &mut Defrag<C>) {
    assert!(defrag.in_progress_tasks_count > 0);
    defrag.in_progress_tasks_count -= 1;
}
