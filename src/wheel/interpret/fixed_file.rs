use std::{
    io,
    path::{
        Path,
        PathBuf,
    },
    time::{
        Instant,
        Duration,
    },
};

use futures::{
    stream::{
        FuturesUnordered,
    },
    channel::{
        mpsc,
        oneshot,
    },
    select,
    SinkExt,
    StreamExt,
};

use tokio::{
    fs,
    io::{
        AsyncSeekExt,
        AsyncReadExt,
        AsyncWriteExt,
    },
};

use alloc_pool::bytes::{
    Bytes,
    BytesMut,
};

use edeltraud::{
    Edeltraud,
};

use crate::{
    job,
    context::Context,
    wheel::{
        block,
        storage,
        core::{
            task,
            performer,
        },
    },
    InterpretStats,
};

use super::{
    Request,
    RequestTask,
    RequestReplyRx,
    DoneTask,
};

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub enum Error {
    WheelFileInitialSeek(io::Error),
    WheelFileSeek {
        offset: u64,
        cursor: u64,
        error: io::Error,
    },
    BlockHeaderSerialize(bincode::Error),
    CommitTagSerialize(bincode::Error),
    TombstoneTagSerialize(bincode::Error),
    BlockWrite(io::Error),
    BlockRead(io::Error),
    BlockHeaderDeserialize(bincode::Error),
    CommitTagDeserialize(bincode::Error),
    CorruptedData(CorruptedDataError),
    WheelPeerLost,
    DeviceSyncFlush(io::Error),
    ThreadPoolGone,
}

#[derive(Debug)]
pub enum CorruptedDataError {
    BlockIdMismatch {
        offset: u64,
        block_id_expected: block::Id,
        block_id_actual: block::Id,
    },
    BlockSizeMismatch {
        offset: u64,
        block_id: block::Id,
        block_size_expected: usize,
        block_size_actual: usize,
    },
    CommitTagBlockIdMismatch {
        offset: u64,
        block_id_expected: block::Id,
        block_id_actual: block::Id,
    },
    CommitTagCrcMismatch {
        offset: u64,
        crc_expected: u64,
        crc_actual: u64,
    },
}

#[derive(Debug)]
pub enum WheelCreateError {
    FileCreate {
        wheel_filename: PathBuf,
        error: io::Error,
    },
    InitWheelSizeIsTooSmall {
        provided: usize,
        required_min: usize,
    },
    HeaderSerialize(bincode::Error),
    HeaderTagWrite(io::Error),
    ZeroChunkWrite(io::Error),
    Flush(io::Error),
}

#[derive(Debug)]
pub enum WheelOpenError {
    FileWrongType,
    FileMetadata {
        wheel_filename: PathBuf,
        error: io::Error,
    },
    FileOpen {
        wheel_filename: PathBuf,
        error: io::Error,
    },
    HeaderRead(io::Error),
    HeaderDeserialize(bincode::Error),
    HeaderInvalidMagic {
        provided: u64,
        expected: u64,
    },
    HeaderVersionMismatch {
        provided: usize,
        expected: usize,
    },
    WheelSizeMismatch {
        header: u64,
        actual: u64,
    },
    LocateBlock(io::Error),
    BlockSizeTooLarge {
        work_block_size_bytes: usize,
        block_size: usize,
    },
    BlockSeekCommitTag(io::Error),
    BlockRewindCommitTag(io::Error),
    BlockReadCommitTag(io::Error),
    CommitTagDeserialize(bincode::Error),
    BlockSeekContents(io::Error),
    BlockReadContents(io::Error),
    BlockCrcMismatch {
        commit_tag_crc: u64,
        block_crc: u64,
    },
    BlockSeekEnd(io::Error),
}

pub struct WheelData<C> where C: Context {
    pub gen_server: GenServer<C>,
    pub performer: performer::Performer<C>,
}

pub enum WheelOpenStatus<C> where C: Context {
    Success(WheelData<C>),
    FileNotFound {
        performer_builder: performer::PerformerBuilderInit<C>,
    },
}

#[derive(Clone, Debug)]
pub struct CreateParams<P> {
    pub wheel_filename: P,
    pub init_wheel_size_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct OpenParams<P> {
    pub wheel_filename: P,
}

pub struct GenServer<C> where C: Context {
    wheel_file: fs::File,
    work_block: Vec<u8>,
    request_tx: mpsc::Sender<Command<C>>,
    request_rx: mpsc::Receiver<Command<C>>,
    storage_layout: storage::Layout,
}

impl<C> GenServer<C> where C: Context {
    pub async fn create<P>(
        params: CreateParams<P>,
        mut performer_builder: performer::PerformerBuilderInit<C>,
    )
        -> Result<WheelData<C>, WheelCreateError> where P: AsRef<Path>
    {
        log::debug!("creating new wheel file [ {:?} ]", params.wheel_filename.as_ref());

        let mut wheel_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(params.wheel_filename.as_ref())
            .await
            .map_err(|error| WheelCreateError::FileCreate {
                wheel_filename: params.wheel_filename.as_ref().to_owned(),
                error,
            })?;

        let wheel_header = storage::WheelHeader {
            size_bytes: params.init_wheel_size_bytes as u64,
            ..storage::WheelHeader::default()
        };
        bincode::serialize_into(performer_builder.work_block_cleared(), &wheel_header)
            .map_err(WheelCreateError::HeaderSerialize)?;

        let mut cursor = performer_builder.work_block().len();
        let min_wheel_file_size = performer_builder.storage_layout().wheel_header_size;
        assert_eq!(cursor, min_wheel_file_size);
        wheel_file.write_all(performer_builder.work_block()).await
            .map_err(WheelCreateError::HeaderTagWrite)?;

        let work_block_size_bytes = performer_builder.work_block().capacity();
        let size_bytes_total = params.init_wheel_size_bytes;
        if size_bytes_total < min_wheel_file_size {
            return Err(WheelCreateError::InitWheelSizeIsTooSmall {
                provided: size_bytes_total,
                required_min: min_wheel_file_size,
            });
        }

        performer_builder
            .work_block_cleared()
            .extend((0 .. work_block_size_bytes).map(|_| 0));

        while cursor < size_bytes_total {
            let bytes_remain = size_bytes_total - cursor;
            let write_amount = if bytes_remain < work_block_size_bytes {
                bytes_remain
            } else {
                work_block_size_bytes
            };
            wheel_file.write_all(&performer_builder.work_block()[.. write_amount]).await
                .map_err(WheelCreateError::ZeroChunkWrite)?;
            cursor += write_amount;
        }
        wheel_file.flush().await
            .map_err(WheelCreateError::Flush)?;

        log::debug!("interpret::fixed_file create success");
        let storage_layout = performer_builder.storage_layout().clone();

        let (request_tx, request_rx) = mpsc::channel(0);

        let (performer_builder, work_block) = performer_builder.start_fill();

        Ok(WheelData {
            gen_server: GenServer {
                wheel_file,
                work_block,
                request_tx,
                request_rx,
                storage_layout,
            },
            performer: performer_builder
                .finish(params.init_wheel_size_bytes),
        })
    }

    pub async fn open<P>(
        params: OpenParams<P>,
        mut performer_builder: performer::PerformerBuilderInit<C>,
    )
        -> Result<WheelOpenStatus<C>, WheelOpenError> where P: AsRef<Path>
    {
        log::debug!("opening existing wheel file [ {:?} ]", params.wheel_filename.as_ref());

        let file_size = match fs::metadata(&params.wheel_filename).await {
            Ok(ref metadata) if metadata.file_type().is_file() =>
                metadata.len(),
            Ok(..) =>
                return Err(WheelOpenError::FileWrongType),
            Err(ref error) if error.kind() == io::ErrorKind::NotFound =>
                return Ok(WheelOpenStatus::FileNotFound { performer_builder, }),
            Err(error) =>
                return Err(WheelOpenError::FileMetadata {
                    wheel_filename: params.wheel_filename.as_ref().to_owned(),
                    error,
                }),
        };

        let mut wheel_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(params.wheel_filename.as_ref())
            .await
            .map_err(|error| WheelOpenError::FileOpen {
                wheel_filename: params.wheel_filename.as_ref().to_owned(),
                error,
            })?;

        let wheel_header_size = performer_builder
            .storage_layout()
            .wheel_header_size;

        // read wheel header
        performer_builder
            .work_block_cleared()
            .extend((0 .. wheel_header_size).map(|_| 0));
        wheel_file.read_exact(performer_builder.work_block()).await
            .map_err(WheelOpenError::HeaderRead)?;
        let wheel_header: storage::WheelHeader = bincode::deserialize_from(&performer_builder.work_block()[..])
            .map_err(WheelOpenError::HeaderDeserialize)?;
        if wheel_header.magic != storage::WHEEL_MAGIC {
            return Err(WheelOpenError::HeaderInvalidMagic {
                provided: wheel_header.magic,
                expected: storage::WHEEL_MAGIC,
            });
        }
        if wheel_header.version != storage::WHEEL_VERSION {
            return Err(WheelOpenError::HeaderVersionMismatch {
                provided: wheel_header.version,
                expected: storage::WHEEL_VERSION,
            });
        }
        if wheel_header.size_bytes != file_size {
            return Err(WheelOpenError::WheelSizeMismatch {
                header: wheel_header.size_bytes,
                actual: file_size,
            });
        }

        // read blocks and gaps
        let (mut builder, mut work_block) = performer_builder.start_fill();

        work_block.clear();
        let mut cursor = wheel_header_size as u64;

        let work_block_size_bytes = work_block.capacity();
        work_block.resize(work_block_size_bytes, 0);
        let mut offset = 0;
        loop {
            let bytes_read = match wheel_file.read(&mut work_block[offset ..]).await {
                Ok(0) =>
                    break,
                Ok(bytes_read) =>
                    bytes_read,
                Err(ref error) if error.kind() == io::ErrorKind::Interrupted =>
                    continue,
                Err(error) =>
                    return Err(WheelOpenError::LocateBlock(error)),
            };
            offset += bytes_read;
            let mut start = 0;
            while offset - start >= builder.storage_layout().block_header_size {
                let area = &work_block[start .. start + builder.storage_layout().block_header_size];
                match bincode::deserialize_from::<_, storage::BlockHeader>(area) {
                    Ok(block_header) if block_header.magic == storage::BLOCK_MAGIC => {
                        let try_read_block_status = try_read_block(
                            &mut wheel_file,
                            &mut work_block,
                            cursor,
                            &block_header,
                            builder.storage_layout(),
                        ).await?;
                        work_block.resize(work_block_size_bytes, 0);
                        offset = 0;
                        start = 0;

                        match try_read_block_status {
                            ReadBlockStatus::NotABlock { next_cursor, } =>
                                cursor = next_cursor,
                            ReadBlockStatus::BlockFound { next_cursor, } => {
                                builder.push_block(cursor, block_header);
                                cursor = next_cursor;
                            },
                        }
                        break;
                    },
                    Ok(..) | Err(..) =>
                        (),
                };
                start += 1;
                cursor += 1;
            }
            if start > 0 {
                work_block.copy_within(start .. offset, 0);
                offset -= start;
            }
        }
        assert!(
            cursor + builder.storage_layout().block_header_size as u64 >= file_size,
            "assertion failed: cursor = {} + block_header_size = {} >= file_size = {}",
            cursor,
            builder.storage_layout().block_header_size,
            file_size,
        );

        log::debug!("loaded wheel schema");

        let (request_tx, request_rx) = mpsc::channel(0);

        Ok(WheelOpenStatus::Success(WheelData {
            gen_server: GenServer {
                wheel_file,
                work_block,
                request_tx,
                request_rx,
                storage_layout: builder
                    .storage_layout()
                    .clone(),
            },
            performer: builder
                .finish(wheel_header.size_bytes as usize),
        }))
    }

    pub fn pid(&self) -> Pid<C> {
        Pid {
            request_tx: self.request_tx.clone(),
        }
    }

    pub async fn run<J>(self, thread_pool: Edeltraud<J>) -> Result<(), Error>
    where C: Send, J: edeltraud::Job + From<job::Job>,
          J::Output: From<job::JobOutput>,
          job::JobOutput: From<J::Output>,
    {
        busyloop(
            self.request_rx,
            self.wheel_file,
            self.work_block,
            self.storage_layout,
            thread_pool,
        ).await
    }
}

#[derive(Clone)]
pub struct Pid<C> where C: Context {
    request_tx: mpsc::Sender<Command<C>>,
}

impl<C> Pid<C> where C: Context {
    pub async fn push_request(&mut self, offset: u64, task: RequestTask<C>) -> Result<RequestReplyRx<C>, ero::NoProcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(Command::Request(Request { offset, task, reply_tx, }))
            .await
            .map_err(|_send_error| ero::NoProcError)?;
        Ok(reply_rx)
    }

    pub async fn device_sync(&mut self) -> Result<Synced, ero::NoProcError> {
        loop {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.request_tx.send(Command::DeviceSync { reply_tx, }).await
                .map_err(|_send_error| ero::NoProcError)?;
            match reply_rx.await {
                Ok(Synced) =>
                    return Ok(Synced),
                Err(oneshot::Canceled) =>
                    (),
            }
        }
    }
}

pub struct Synced;

enum Command<C> where C: Context {
    Request(Request<C>),
    DeviceSync { reply_tx: oneshot::Sender<Synced>, },
}

enum ReadBlockStatus {
    NotABlock { next_cursor: u64, },
    BlockFound { next_cursor: u64, },
}

async fn try_read_block(
    wheel_file: &mut fs::File,
    work_block: &mut Vec<u8>,
    cursor: u64,
    block_header: &storage::BlockHeader,
    storage_layout: &storage::Layout,
)
    -> Result<ReadBlockStatus, WheelOpenError>
{
    // seek to commit tag position
    wheel_file.seek(io::SeekFrom::Start(cursor + storage_layout.block_header_size as u64 + block_header.block_size as u64)).await
        .map_err(WheelOpenError::BlockSeekCommitTag)?;
    // read commit tag
    work_block.resize(storage_layout.commit_tag_size, 0);
    wheel_file.read_exact(work_block).await
        .map_err(WheelOpenError::BlockReadCommitTag)?;
    let commit_tag: storage::CommitTag = bincode::deserialize_from(&work_block[..])
        .map_err(WheelOpenError::CommitTagDeserialize)?;
    if commit_tag.magic != storage::COMMIT_TAG_MAGIC {
        // not a block: rewind and step
        let next_cursor = cursor + 1;
        wheel_file.seek(io::SeekFrom::Start(next_cursor)).await
            .map_err(WheelOpenError::BlockRewindCommitTag)?;
        return Ok(ReadBlockStatus::NotABlock { next_cursor, });
    }
    if commit_tag.block_id != block_header.block_id {
        // some other block terminator: rewind
        let next_cursor = cursor + 1;
        wheel_file.seek(io::SeekFrom::Start(next_cursor)).await
            .map_err(WheelOpenError::BlockRewindCommitTag)?;
        return Ok(ReadBlockStatus::NotABlock { next_cursor, });
    }
    if block_header.block_size > work_block.capacity() {
        return Err(WheelOpenError::BlockSizeTooLarge {
            work_block_size_bytes: work_block.capacity(),
            block_size: block_header.block_size,
        });
    }
    // seek to block contents
    wheel_file.seek(io::SeekFrom::Start(cursor + storage_layout.block_header_size as u64)).await
        .map_err(WheelOpenError::BlockSeekContents)?;
    // read block contents
    work_block.resize(block_header.block_size, 0);
    wheel_file.read_exact(work_block).await
        .map_err(WheelOpenError::BlockReadContents)?;
    let crc = block::crc(work_block);
    if crc != commit_tag.crc {
        return Err(WheelOpenError::BlockCrcMismatch {
            commit_tag_crc: commit_tag.crc,
            block_crc: crc,
        });
    }
    // seek to the end of commit tag
    let next_cursor = wheel_file.seek(io::SeekFrom::Current(storage_layout.commit_tag_size as i64)).await
        .map_err(WheelOpenError::BlockSeekEnd)?;
    Ok(ReadBlockStatus::BlockFound { next_cursor, })
}

#[derive(Debug, Default)]
struct Timings {
    event_wait: Duration,
    seek: Duration,
    write_prepare: Duration,
    write_write: Duration,
    read: Duration,
    write_delete: Duration,
    flush: Duration,
    total: Duration,
}

async fn busyloop<C, J>(
    request_rx: mpsc::Receiver<Command<C>>,
    mut wheel_file: fs::File,
    mut work_block: Vec<u8>,
    storage_layout: storage::Layout,
    thread_pool: Edeltraud<J>,
)
    -> Result<(), Error>
where C: Context + Send,
      J: edeltraud::Job + From<job::Job>,
      J::Output: From<job::JobOutput>,
      job::JobOutput: From<J::Output>,
{
    let mut stats = InterpretStats::default();
    let mut tasks = FuturesUnordered::new();
    let mut tasks_count = 0;

    let mut fused_request_rx = request_rx.fuse();

    let mut cursor = storage_layout.wheel_header_size as u64;
    wheel_file.seek(io::SeekFrom::Start(cursor)).await
        .map_err(Error::WheelFileInitialSeek)?;

    let mut timings = Timings::default();
    loop {
        let now_loop = Instant::now();

        enum Event<C, T> { Command(C), Task(T), }

        let event = match tasks_count {
            0 =>
                Event::Command(fused_request_rx.next().await),
            _ =>
                select! {
                    result = fused_request_rx.next() =>
                        Event::Command(result),
                    result = tasks.next() => match result {
                        None =>
                            unreachable!(),
                        Some(task) => {
                            tasks_count -= 1;
                            Event::Task(task)
                        },
                    },
                },
        };
        timings.event_wait += now_loop.elapsed();

        match event {

            Event::Command(None) =>
                break,

            Event::Command(Some(Command::Request(Request { offset, task, reply_tx, }))) => {
                stats.count_total += 1;

                if cursor != offset {
                    if cursor < offset {
                        stats.count_seek_forward += 1;
                    } else if cursor > offset {
                        stats.count_seek_backward += 1;
                    }
                    let now = Instant::now();
                    wheel_file.seek(io::SeekFrom::Start(offset)).await
                        .map_err(|error| Error::WheelFileSeek { offset, cursor, error, })?;
                    timings.seek += now.elapsed();
                    cursor = offset;
                } else {
                    stats.count_no_seek += 1;
                }

                match task.kind {
                    task::TaskKind::WriteBlock(write_block) => {
                        let now = Instant::now();
                        let block_header = storage::BlockHeader {
                            block_id: task.block_id.clone(),
                            block_size: write_block.block_bytes.len(),
                            ..Default::default()
                        };
                        work_block.clear();
                        bincode::serialize_into(&mut work_block, &block_header)
                            .map_err(Error::BlockHeaderSerialize)?;
                        work_block.extend_from_slice(&write_block.block_bytes);
                        let commit_tag = storage::CommitTag {
                            block_id: task.block_id.clone(),
                            crc: write_block.block_crc.unwrap(), // must be already calculated
                            ..Default::default()
                        };
                        bincode::serialize_into(&mut work_block, &commit_tag)
                            .map_err(Error::CommitTagSerialize)?;
                        timings.write_prepare += now.elapsed();

                        let now = Instant::now();
                        wheel_file.write_all(&work_block).await
                            .map_err(Error::BlockWrite)?;
                        timings.write_write += now.elapsed();

                        cursor += work_block.len() as u64;

                        let task_done = task::Done {
                            current_offset: cursor,
                            task: task::TaskDone {
                                block_id: task.block_id,
                                kind: task::TaskDoneKind::WriteBlock(task::TaskDoneWriteBlock {
                                    context: write_block.context,
                                }),
                            },
                        };
                        if let Err(_send_error) = reply_tx.send(DoneTask { task_done, stats, }) {
                            break;
                        }
                    },

                    task::TaskKind::ReadBlock(task::ReadBlock { block_header, mut block_bytes, context, }) => {
                        let total_chunk_size = storage_layout.data_size_block_min()
                            + block_header.block_size;
                        block_bytes.resize(total_chunk_size, 0);
                        let now = Instant::now();
                        wheel_file.read_exact(&mut block_bytes).await
                            .map_err(Error::BlockRead)?;
                        timings.read += now.elapsed();
                        cursor += block_bytes.len() as u64;

                        let storage_layout = storage_layout.clone();
                        let block_process_task = thread_pool.spawn(job::Job::BlockProcess(BlockProcessJobArgs {
                            offset,
                            storage_layout: storage_layout.clone(),
                            block_header,
                            block_bytes,
                        }));

                        // moving block process to separate task, unlock main loop
                        tasks.push(async move {
                            let job_output = block_process_task.await
                                .map_err(|edeltraud::SpawnError::ThreadPoolGone| Error::ThreadPoolGone)?;
                            let job_output: job::JobOutput = job_output.into();
                            let job::BlockProcessDone(block_process_result) = job_output.into();
                            let BlockProcessJobDone { block_id, block_bytes, block_crc, } = block_process_result?;

                            let task_done = task::Done {
                                current_offset: cursor,
                                task: task::TaskDone {
                                    block_id,
                                    kind: task::TaskDoneKind::ReadBlock(task::TaskDoneReadBlock {
                                        block_bytes,
                                        block_crc,
                                        context,
                                    }),
                                },
                            };

                            reply_tx.send(DoneTask { task_done, stats, })
                                .map_err(|_send_error| Error::WheelPeerLost)
                        });
                        tasks_count += 1;
                    },

                    task::TaskKind::DeleteBlock(delete_block) => {
                        let tombstone_tag = storage::TombstoneTag::default();
                        work_block.clear();
                        bincode::serialize_into(&mut work_block, &tombstone_tag)
                            .map_err(Error::TombstoneTagSerialize)?;

                        let now = Instant::now();
                        wheel_file.write_all(&work_block).await
                            .map_err(Error::BlockWrite)?;
                        timings.write_delete += now.elapsed();
                        cursor += work_block.len() as u64;

                        let task_done = task::Done {
                            current_offset: cursor,
                            task: task::TaskDone {
                                block_id: task.block_id,
                                kind: task::TaskDoneKind::DeleteBlock(task::TaskDoneDeleteBlock {
                                    context: delete_block.context,
                                }),
                            },
                        };
                        if let Err(_send_error) = reply_tx.send(DoneTask { task_done, stats, }) {
                            break;
                        }
                    },
                }
            },

            Event::Command(Some(Command::DeviceSync { reply_tx, })) => {
                let now = Instant::now();
                wheel_file.flush().await
                    .map_err(Error::DeviceSyncFlush)?;
                timings.flush += now.elapsed();
                if let Err(_send_error) = reply_tx.send(Synced) {
                    break;
                }
                log::info!("current timings: {:?}", timings);
            },

            Event::Task(Err(Error::WheelPeerLost)) =>
                break,

            Event::Task(task_result) => {
                let () = task_result?;
            },

        }
        timings.total += now_loop.elapsed();
    }

    log::debug!("master channel closed in interpret_loop, shutting down");
    Ok(())
}

pub type BlockProcessJobOutput = Result<BlockProcessJobDone, Error>;

pub struct BlockProcessJobDone {
    block_id: block::Id,
    block_bytes: Bytes,
    block_crc: u64,
}

pub struct BlockProcessJobArgs {
    offset: u64,
    storage_layout: storage::Layout,
    block_header: storage::BlockHeader,
    block_bytes: BytesMut,
}

pub fn block_process_job(
    BlockProcessJobArgs {
        offset,
        storage_layout,
        block_header,
        block_bytes,
    }: BlockProcessJobArgs,
)
    -> BlockProcessJobOutput
{
    let block_buffer_start = storage_layout.block_header_size;
    let block_buffer_end = block_bytes.len() - storage_layout.commit_tag_size;

    let storage_block_header: storage::BlockHeader = bincode::deserialize_from(
        &block_bytes[.. block_buffer_start],
    ).map_err(Error::BlockHeaderDeserialize)?;
    if storage_block_header.block_id != block_header.block_id {
        return Err(Error::CorruptedData(CorruptedDataError::BlockIdMismatch {
            offset,
            block_id_expected: block_header.block_id,
            block_id_actual: storage_block_header.block_id,
        }));
    }

    if storage_block_header.block_size != block_header.block_size {
        return Err(Error::CorruptedData(CorruptedDataError::BlockSizeMismatch {
            offset,
            block_id: block_header.block_id,
            block_size_expected: block_header.block_size,
            block_size_actual: storage_block_header.block_size,
        }));
    }
    let commit_tag: storage::CommitTag = bincode::deserialize_from(
        &block_bytes[block_buffer_end ..],
    ).map_err(Error::CommitTagDeserialize)?;
    if commit_tag.block_id != block_header.block_id {
        return Err(Error::CorruptedData(CorruptedDataError::CommitTagBlockIdMismatch {
            offset,
            block_id_expected: block_header.block_id,
            block_id_actual: commit_tag.block_id,
        }));
    }
    let block_bytes = block_bytes.freeze_range(block_buffer_start .. block_buffer_end);
    let block_id = block_header.block_id;

    let crc_expected = block::crc(&block_bytes);
    if commit_tag.crc != crc_expected {
        return Err(Error::CorruptedData(CorruptedDataError::CommitTagCrcMismatch {
            offset,
            crc_expected,
            crc_actual: commit_tag.crc,
        }));
    }

    Ok(BlockProcessJobDone { block_id, block_bytes, block_crc: commit_tag.crc, })
}
