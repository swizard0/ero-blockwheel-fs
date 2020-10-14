use std::mem;

use super::{
    super::{
        super::{
            Info,
            context::Context,
        },
        lru,
        pool,
        proto,
    },
    task,
    block,
    schema,
    defrag,
    SpaceKey,
    BlockGet,
    BlockEntry,
    BlockEntryGet,
};

#[cfg(test)]
mod tests;

struct Inner<C> where C: Context {
    schema: schema::Schema,
    lru_cache: lru::Cache,
    blocks_pool: pool::Blocks,
    defrag: Option<Defrag<C::WriteBlock>>,
    bg_task: BackgroundTask<C::Interpreter>,
    tasks_queue: task::queue::Queue<C>,
    done_task: DoneTask,
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
        block_bytes: block::Bytes,
    },
    DeleteBlockRegular {
        block_id: block::Id,
        block_entry: BlockEntry,
        freed_space_key: SpaceKey,
    },
    DeleteBlockDefrag {
        block_id: block::Id,
        block_bytes: block::Bytes,
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
    LendBlock(TaskDoneOp<C::LendBlock, LendBlockOp>),
    WriteBlock(TaskDoneOp<C::WriteBlock, WriteBlockOp>),
    ReadBlock(TaskDoneOp<C::ReadBlock, ReadBlockOp>),
    DeleteBlock(TaskDoneOp<C::DeleteBlock, DeleteBlockOp>),
}

pub struct TaskDoneOp<C, O> {
    pub context: C,
    pub op: O,
}

pub enum InfoOp {
    Success { info: Info, },
}

pub enum LendBlockOp {
    Success { block_bytes: block::BytesMut, },
}

pub enum WriteBlockOp {
    NoSpaceLeft,
    Done { block_id: block::Id, },
}

pub enum ReadBlockOp {
    NotFound,
    Done { block_bytes: block::Bytes, },
}

pub enum DeleteBlockOp {
    NotFound,
    Done { block_id: block::Id, },
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

impl<C> Performer<C> where C: Context {
    pub fn new(
        schema: schema::Schema,
        lru_cache: lru::Cache,
        defrag_queues: Option<DefragConfig<C::WriteBlock>>,
    )
        -> Performer<C>
    {
        Performer {
            inner: Inner::new(
                schema,
                lru_cache,
                defrag_queues
                    .map(|config| Defrag {
                        queues: config.queues,
                        in_progress_tasks_count: 0,
                        in_progress_tasks_limit: config.in_progress_tasks_limit,
                    }),
            ),
        }
    }

    pub fn next(self) -> Op<C> {
        self.inner.incoming_poke()
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

    pub fn incoming_task_done(self, task_done: task::Done<C>) -> Op<C> {
        self.inner.incoming_interpreter(task_done)
    }
}

impl<C> PollRequestNext<C> where C: Context {
    pub fn incoming_request(self, request: proto::Request<C>) -> Op<C> {
        self.inner.incoming_request(request)
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
    fn new(schema: schema::Schema, lru_cache: lru::Cache, defrag: Option<Defrag<C::WriteBlock>>) -> Inner<C> {
        Inner {
            schema,
            lru_cache,
            blocks_pool: pool::Blocks::new(),
            tasks_queue: task::queue::Queue::new(),
            defrag,
            bg_task: BackgroundTask {
                current_offset: 0,
                state: BackgroundTaskState::Idle,
            },
            done_task: DoneTask::None,
        }
    }

    fn incoming_poke(mut self) -> Op<C> {
        match mem::replace(&mut self.done_task, DoneTask::None) {
            DoneTask::None =>
                (),
            DoneTask::ReadBlock { block_id, block_bytes, } => {
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                assert!(lens.pop_write_task(self.schema.block_get()).is_none());
                if let Some(read_block) = lens.pop_read_task(self.schema.block_get()) {
                    self.blocks_pool.repay(read_block.block_bytes.freeze());
                    self.done_task = DoneTask::ReadBlock {
                        block_id: block_id.clone(),
                        block_bytes: block_bytes.clone(),
                    };
                    return self.proceed_read_block_task_done(block_id, block_bytes, read_block.context);
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
                        task::WriteBlockContext::Defrag { .. } =>
                        // cancel defrag write task
                            (),
                    }
                }
                while let Some(read_block) = lens.pop_read_task(&mut block_get) {
                    self.blocks_pool.repay(read_block.block_bytes.freeze());
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
                        task::ReadBlockContext::Defrag { .. } =>
                        // cancel defrag read task
                            (),
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
                        task::DeleteBlockContext::Defrag { .. } =>
                        // cancel defrag delete task
                            (),
                    }
                }
                self.flush_defrag_pending_queue(Some(freed_space_key));
            },
            DoneTask::DeleteBlockDefrag { block_id, block_bytes, freed_space_key, } => {
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                while let Some(read_block) = lens.pop_read_task(self.schema.block_get()) {
                    self.blocks_pool.repay(read_block.block_bytes.freeze());
                    match read_block.context {
                        task::ReadBlockContext::External(context) => {
                            self.done_task = DoneTask::DeleteBlockDefrag {
                                block_id: block_id.clone(),
                                block_bytes: block_bytes.clone(),
                                freed_space_key,
                            };
                            return Op::Event(Event {
                                op: EventOp::ReadBlock(TaskDoneOp {
                                    context,
                                    op: ReadBlockOp::Done { block_bytes, },
                                }),
                                performer: Performer { inner: self, },
                            });
                            },
                        task::ReadBlockContext::Defrag { .. } =>
                            unreachable!(),
                    }
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
                if let Some((_free_space_offset, space_key)) = defrag.queues.tasks.pop() {
                    if let Some(block_entry) = self.schema.pick_defrag_space_key(&space_key) {

                        println!("   /// DEFRAG scheduling read for {:?}", block_entry.header.block_id);

                        let block_bytes = self.blocks_pool.lend();
                        let mut lens = self.tasks_queue.focus_block_id(block_entry.header.block_id.clone());
                        lens.push_task(
                            task::Task {
                                block_id: block_entry.header.block_id.clone(),
                                kind: task::TaskKind::ReadBlock(task::ReadBlock {
                                    block_header: block_entry.header.clone(),
                                    block_bytes,
                                    context: task::ReadBlockContext::Defrag { space_key, },
                                }),
                            },
                            self.schema.block_get(),
                        );
                        lens.enqueue(self.schema.block_get());
                        defrag.in_progress_tasks_count += 1;
                    }
                } else {
                    break;
                }
            }
        }

        match mem::replace(&mut self.bg_task.state, BackgroundTaskState::Idle) {
            BackgroundTaskState::Idle =>
                if let Some((offset, mut lens)) = self.tasks_queue.next_trigger(self.bg_task.current_offset, self.schema.block_get()) {
                    let task_kind = match lens.pop_task(self.schema.block_get()) {
                        Some(task_kind) => task_kind,
                        None => panic!("empty task queue unexpected for block {:?} @ {}", lens.block_id(), offset),
                    };
                    self.bg_task.state = BackgroundTaskState::Await {
                        block_id: lens.block_id().clone(),
                    };

                    Op::Query(QueryOp::InterpretTask(InterpretTask {
                        offset,
                        task: task::Task {
                            block_id: lens.block_id().clone(),
                            kind: task_kind,
                        },
                        next: InterpretTaskNext {
                            inner: self,
                        },
                    }))
                } else {
                    Op::Query(QueryOp::PollRequest(PollRequest {
                        next: PollRequestNext {
                            inner: self,
                        },
                    }))
                },
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
            proto::Request::LendBlock(request_lend_block) =>
                self.incoming_request_lend_block(request_lend_block),
            proto::Request::RepayBlock(request_repay_block) =>
                self.incoming_request_repay_block(request_repay_block),
            proto::Request::WriteBlock(request_write_block) =>
                self.incoming_request_write_block(request_write_block),
            proto::Request::ReadBlock(request_read_block) =>
                self.incoming_request_read_block(request_read_block),
            proto::Request::DeleteBlock(request_delete_block) =>
                self.incoming_request_delete_block(request_delete_block),
        }
    }

    fn incoming_request_info(self, proto::RequestInfo { context, }: proto::RequestInfo<C::Info>) -> Op<C> {
        let mut info = self.schema.info();
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

    fn incoming_request_lend_block(mut self, proto::RequestLendBlock { context, }: proto::RequestLendBlock<C::LendBlock>) -> Op<C> {
        let block_bytes = self.blocks_pool.lend();
        Op::Event(Event {
            op: EventOp::LendBlock(TaskDoneOp { context, op: LendBlockOp::Success { block_bytes, }, }),
            performer: Performer { inner: self, },
        })
    }

    fn incoming_request_repay_block(mut self, proto::RequestRepayBlock { block_bytes, }: proto::RequestRepayBlock) -> Op<C> {
        self.blocks_pool.repay(block_bytes);
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

            schema::ReadBlockOp::Cached { block_bytes, } =>
                Op::Event(Event {
                    op: EventOp::ReadBlock(TaskDoneOp {
                        context: request_read_block.context,
                        op: ReadBlockOp::Done { block_bytes, },
                    }),
                    performer: Performer { inner: self, },
                }),

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

    fn incoming_interpreter(mut self, incoming: task::Done<C>) -> Op<C> {
        match incoming {

            task::Done { current_offset, task: task::TaskDone { block_id, kind: task::TaskDoneKind::WriteBlock(write_block), }, } => {
                self.bg_task = BackgroundTask { current_offset, state: BackgroundTaskState::Idle, };
                let mut lens = self.tasks_queue.focus_block_id(block_id.clone());
                lens.finish(self.schema.block_get());
                lens.enqueue(self.schema.block_get());
                self.schema.process_write_block_task_done(&block_id);
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

                        println!("   /// DEFRAG write task done for {:?}", block_id);

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
                let block_bytes = read_block.block_bytes.freeze();
                self.lru_cache.insert(block_id.clone(), block_bytes.clone());
                self.done_task = DoneTask::ReadBlock {
                    block_id: block_id.clone(),
                    block_bytes: block_bytes.clone(),
                };
                self.proceed_read_block_task_done(block_id, block_bytes, read_block.context)
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
                                match (defrag_op, self.defrag.as_mut()) {
                                    (
                                        schema::DefragOp::Queue { free_space_offset, space_key, },
                                        Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }),
                                    ) =>
                                        tasks.push(free_space_offset, space_key),
                                    (schema::DefragOp::None, _) | (_, None) =>
                                        (),
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
                    task::DeleteBlockContext::Defrag { space_key, } =>
                        match self.schema.process_delete_block_task_done_defrag(block_id.clone(), space_key) {
                            schema::DeleteBlockTaskDoneDefragOp::Perform(task_op) => {

                                println!("   /// DEFRAG delete task done for {:?}, scheduling write", block_id);

                                match (task_op.defrag_op, self.defrag.as_mut()) {
                                    (
                                        schema::DefragOp::Queue { free_space_offset, space_key, },
                                        Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }),
                                    ) =>
                                        tasks.push(free_space_offset, space_key),
                                    (schema::DefragOp::None, _) | (_, None) =>
                                        (),
                                }
                                self.tasks_queue.focus_block_id(block_id.clone())
                                    .push_task(
                                        task::Task {
                                            block_id: block_id.clone(),
                                            kind: task::TaskKind::WriteBlock(task::WriteBlock {
                                                block_bytes: task_op.block_bytes.clone(),
                                                context: task::WriteBlockContext::Defrag,
                                            }),
                                        },
                                        self.schema.block_get(),
                                    );
                                self.done_task = DoneTask::DeleteBlockDefrag {
                                    block_id,
                                    block_bytes: task_op.block_bytes,
                                    freed_space_key: task_op.freed_space_key,
                                };
                                Op::Idle(Performer { inner: self, })
                            },
                        },
                }
            },

        }
    }

    fn proceed_read_block_task_done(
        mut self,
        block_id: block::Id,
        block_bytes: block::Bytes,
        task_context: task::ReadBlockContext<C::ReadBlock>,
    ) -> Op<C> {
        match self.schema.process_read_block_task_done(&block_id) {
            schema::ReadBlockTaskDoneOp::Perform(schema::ReadBlockTaskDonePerform { block_bytes_cached, }) =>
                match task_context {
                    task::ReadBlockContext::External(context) =>
                        Op::Event(Event {
                            op: EventOp::ReadBlock(TaskDoneOp {
                                context,
                                op: ReadBlockOp::Done { block_bytes, },
                            }),
                            performer: Performer { inner: self, },
                        }),
                    task::ReadBlockContext::Defrag { space_key, } => {

                        println!("   /// DEFRAG read task done for {:?}, scheduling delete", block_id);

                        let block_bytes_cached_prev =
                            mem::replace(block_bytes_cached, Some(block_bytes));
                        assert_eq!(block_bytes_cached_prev, None);
                        self.tasks_queue.focus_block_id(block_id.clone())
                            .push_task(
                                task::Task {
                                    block_id,
                                    kind: task::TaskKind::DeleteBlock(task::DeleteBlock {
                                        context: task::DeleteBlockContext::Defrag { space_key, },
                                    }),
                                },
                                self.schema.block_get(),
                            );
                        Op::Idle(Performer { inner: self, })
                    },
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
    match (defrag_op, defrag.as_mut()) {
        (
            schema::DefragOp::Queue { free_space_offset, space_key, },
            Some(Defrag { queues: defrag::Queues { tasks, .. }, .. }),
        ) =>
            tasks.push(free_space_offset, space_key),
        (schema::DefragOp::None, _) | (_, None) =>
            (),
    }
    let mut lens = tasks_queue.focus_block_id(task_op.block_id.clone());
    lens.push_task(
        task::Task {
            block_id: task_op.block_id,
            kind: task::TaskKind::WriteBlock(task::WriteBlock {
                block_bytes: request_write_block.block_bytes,
                context: task::WriteBlockContext::External(
                    request_write_block.context,
                ),
            }),
        },
        &mut block_get,
    );
    lens.enqueue(block_get);
}
