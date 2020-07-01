use super::{
    gaps,
    task,
    proto,
    block,
    index,
    defrag,
    storage,
};

#[derive(Debug)]
pub struct Schema {
    next_block_id: block::Id,
    storage_layout: storage::Layout,
    blocks_index: index::Blocks,
    gaps: gaps::Index,
    defrag_pending_queue: defrag::PendingQueue,
    defrag_task_queue: defrag::TaskQueue,
}

impl Schema {
    pub fn new(storage_layout: storage::Layout) -> Schema {
        Schema {
            next_block_id: block::Id::init(),
            storage_layout,
            blocks_index: index::Blocks::new(),
            gaps: gaps::Index::new(),
            defrag_pending_queue: defrag::PendingQueue::new(),
            defrag_task_queue: defrag::TaskQueue::new(),
        }
    }

    pub fn initialize_empty(&mut self, size_bytes_total: u64) {
        self.blocks_index.insert(
            self.next_block_id.clone(),
            index::BlockEntry {
                offset: self.storage_layout.wheel_header_size as u64,
                header: storage::BlockHeader::EndOfFile,
            },
        );

        let total_service_size = self.storage_layout.data_size_service_min()
            + self.storage_layout.data_size_block_min();
        if size_bytes_total > total_service_size as u64 {
            let space_available = size_bytes_total - total_service_size as u64;
            self.gaps.insert(
                space_available as usize,
                gaps::GapBetween::BlockAndEnd {
                    left_block: self.next_block_id.clone(),
                },
            );
        }

        self.next_block_id = self.next_block_id.next();
    }

    pub fn process_write_block_request(
        &mut self,
        proto::RequestWriteBlock { block_bytes, reply_tx, }: proto::RequestWriteBlock,
        tasks_queue: &mut task::Queue,
    )
    {
        let block_id = self.next_block_id.clone();
        self.next_block_id = self.next_block_id.next();
        let mut task_write_block = task::WriteBlock {
            block_id,
            block_bytes,
            reply_tx,
            state: task::WriteBlockState::Header,
            commit_type: task::CommitType::CommitOnly,
        };

        let space_required = task_write_block.block_bytes.len();

        match self.gaps.allocate(space_required, &self.blocks_index) {
            Ok(gaps::Allocated::Success { space_available, between: gaps::GapBetween::StartAndBlock { right_block, }, }) => {
                let block_offset = self.storage_layout.wheel_header_size as u64;
                let right_block_id = right_block.block_id.clone();
                self.blocks_index.insert(
                    task_write_block.block_id.clone(),
                    index::BlockEntry {
                        offset: block_offset,
                        header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                            block_id: task_write_block.block_id.clone(),
                            block_size: space_required,
                        }),
                    },
                );

                let space_left = space_available - space_required;
                if space_left >= self.storage_layout.data_size_block_min() {
                    let space_left_available = space_left - self.storage_layout.data_size_block_min();
                    self.gaps.insert(
                        space_left_available,
                        gaps::GapBetween::TwoBlocks {
                            left_block: task_write_block.block_id.clone(),
                            right_block: right_block_id.clone(),
                        },
                    );
                }
                if space_left > 0 {
                    let free_space_offset = block_offset
                        + self.storage_layout.data_size_block_min() as u64
                        + space_required as u64;
                    self.defrag_task_queue.push(free_space_offset, defrag::DefragPosition::TwoBlocks {
                        left_block_id: task_write_block.block_id.clone(),
                        right_block_id: right_block_id.clone(),
                    });
                }

                tasks_queue.push(block_offset, task::TaskKind::WriteBlock(task_write_block));
            },
            Ok(gaps::Allocated::Success { space_available, between: gaps::GapBetween::TwoBlocks { left_block, right_block, }, }) => {
                let block_offset = left_block.block_entry.offset
                    + self.storage_layout.data_size_block_min() as u64
                    + left_block.block_entry.header.block_size() as u64;
                let right_block_id = right_block.block_id.clone();
                self.blocks_index.insert(
                    task_write_block.block_id.clone(),
                    index::BlockEntry {
                        offset: block_offset,
                        header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                            block_id: task_write_block.block_id.clone(),
                            block_size: space_required,
                        }),
                    },
                );

                let space_left = space_available - space_required;
                if space_left >= self.storage_layout.data_size_block_min() {
                    let space_left_available = space_left - self.storage_layout.data_size_block_min();
                    self.gaps.insert(
                        space_left_available,
                        gaps::GapBetween::TwoBlocks {
                            left_block: task_write_block.block_id.clone(),
                            right_block: right_block_id.clone(),
                        },
                    );
                }
                if space_left > 0 {
                    let free_space_offset = block_offset
                        + self.storage_layout.data_size_block_min() as u64
                        + space_required as u64;
                    self.defrag_task_queue.push(free_space_offset, defrag::DefragPosition::TwoBlocks {
                        left_block_id: task_write_block.block_id.clone(),
                        right_block_id: right_block_id.clone(),
                    });
                }

                tasks_queue.push(block_offset, task::TaskKind::WriteBlock(task_write_block));
            },
            Ok(gaps::Allocated::Success { space_available, between: gaps::GapBetween::BlockAndEnd { left_block, }, }) => {
                assert_eq!(left_block.block_entry.header, storage::BlockHeader::EndOfFile);
                let block_offset = left_block.block_entry.offset;
                let left_block_id = left_block.block_id.clone();

                let maybe_eof = self.blocks_index.remove(left_block_id);
                assert!(matches!(maybe_eof, Some(index::BlockEntry { header: storage::BlockHeader::EndOfFile, .. })));

                self.blocks_index.insert(
                    task_write_block.block_id.clone(),
                    index::BlockEntry {
                        offset: block_offset,
                        header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                            block_id: task_write_block.block_id.clone(),
                            block_size: space_required,
                        }),
                    },
                );

                let eof_block_id = self.next_block_id.clone();
                self.next_block_id = self.next_block_id.next();
                self.blocks_index.insert(
                    eof_block_id.clone(),
                    index::BlockEntry {
                        offset: block_offset
                            + space_required as u64
                            + self.storage_layout.data_size_block_min() as u64,
                        header: storage::BlockHeader::EndOfFile,
                    },
                );

                let space_left = space_available - space_required;
                if space_left >= self.storage_layout.data_size_block_min() {
                    let space_left_available = space_left - self.storage_layout.data_size_block_min();
                    self.gaps.insert(
                        space_left_available,
                        gaps::GapBetween::BlockAndEnd {
                            left_block: eof_block_id.clone(),
                        },
                    );
                }

                task_write_block.commit_type = task::CommitType::CommitAndEof;
                tasks_queue.push(block_offset, task::TaskKind::WriteBlock(task_write_block));
            },
            Ok(gaps::Allocated::PendingDefragmentation) => {
                log::debug!(
                    "cannot directly allocate {} bytes in process_write_block_request: moving to pending defrag queue",
                    task_write_block.block_bytes.len(),
                );
                self.defrag_pending_queue.push(task_write_block);
            },
            Err(gaps::Error::NoSpaceLeft) => {
                if let Err(_send_error) = task_write_block.reply_tx.send(Err(proto::RequestWriteBlockError::NoSpaceLeft)) {
                    log::warn!("process_write_block_request: reply channel has been closed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::oneshot;

    use super::{
        task,
        proto,
        block,
        index,
        storage,
        Schema,
    };

    fn init() -> (Schema, task::Queue) {
        let storage_layout = storage::Layout {
            wheel_header_size: 24,
            eof_block_header_size: 4,
            regular_block_header_size: 20,
            commit_tag_size: 16,
        };
        let mut schema = Schema::new(storage_layout);
        schema.initialize_empty(128);

        (schema, task::Queue::new())
    }

    fn sample_hello_world() -> block::Bytes {
        let mut block_bytes_mut = block::BytesMut::new();
        block_bytes_mut.extend("hello, world!".as_bytes().iter().cloned());
        block_bytes_mut.freeze()
    }

    #[test]
    fn process_write_block_request() {
        let (mut schema, mut tasks_queue) = init();
        assert_eq!(schema.gaps.space_total(), 64);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        schema.process_write_block_request(
            proto::RequestWriteBlock {
                block_bytes: sample_hello_world(),
                reply_tx,
            },
            &mut tasks_queue,
        );

        assert_eq!(schema.next_block_id, block::Id::init().next().next().next());
        assert_eq!(schema.blocks_index.get(&block::Id::init()), None);
        assert_eq!(
            schema.blocks_index.get(&block::Id::init().next()),
            Some(&index::BlockEntry {
                offset: 24,
                header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                    block_id: block::Id::init().next(),
                    block_size: 13,
                }),
            }),
        );
        assert_eq!(
            schema.blocks_index.get(&block::Id::init().next().next()),
            Some(&index::BlockEntry {
                offset: 73,
                header: storage::BlockHeader::EndOfFile,
            }),
        );
        assert_eq!(schema.gaps.space_total(), 15);
        assert_eq!(reply_rx.try_recv(), Ok(None));

        let (reply_tx, mut reply_rx) = oneshot::channel();
        schema.process_write_block_request(
            proto::RequestWriteBlock {
                block_bytes: sample_hello_world(),
                reply_tx,
            },
            &mut tasks_queue,
        );

        assert_eq!(schema.next_block_id, block::Id::init().next().next().next().next().next());
        assert_eq!(schema.blocks_index.get(&block::Id::init()), None);
        assert_eq!(
            schema.blocks_index.get(&block::Id::init().next()),
            Some(&index::BlockEntry {
                offset: 24,
                header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                    block_id: block::Id::init().next(),
                    block_size: 13,
                }),
            }),
        );
        assert_eq!(schema.blocks_index.get(&block::Id::init().next().next()), None);
        assert_eq!(
            schema.blocks_index.get(&block::Id::init().next().next().next()),
            Some(&index::BlockEntry {
                offset: 73,
                header: storage::BlockHeader::Regular(storage::BlockHeaderRegular {
                    block_id: block::Id::init().next().next().next(),
                    block_size: 13,
                }),
            }),
        );
        assert_eq!(
            schema.blocks_index.get(&block::Id::init().next().next().next().next()),
            Some(&index::BlockEntry {
                offset: 122,
                header: storage::BlockHeader::EndOfFile,
            }),
        );
        assert_eq!(schema.gaps.space_total(), 0);
        assert_eq!(reply_rx.try_recv(), Ok(None));

        let (reply_tx, mut reply_rx) = oneshot::channel();
        schema.process_write_block_request(
            proto::RequestWriteBlock {
                block_bytes: sample_hello_world(),
                reply_tx,
            },
            &mut tasks_queue,
        );
        assert_eq!(reply_rx.try_recv(), Ok(Some(Err(proto::RequestWriteBlockError::NoSpaceLeft))));
    }
}