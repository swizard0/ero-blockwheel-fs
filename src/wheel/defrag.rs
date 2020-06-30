use std::{
    cmp,
    collections::{
        VecDeque,
        BinaryHeap,
    },
};

use super::{
    block,
    task,
};

#[derive(Debug)]
pub struct PendingQueue {
    queue: VecDeque<task::WriteBlock>,
}

impl PendingQueue {
    pub fn new() -> PendingQueue {
        PendingQueue {
            queue: VecDeque::new(),
        }
    }

    pub fn push(&mut self, task_write_block: task::WriteBlock) {
        self.queue.push_back(task_write_block);
    }
}

#[derive(Debug)]
pub struct TaskQueue {
    queue: BinaryHeap<DefragTask>,
}

impl TaskQueue {
    pub fn new() -> TaskQueue {
        TaskQueue {
            queue: BinaryHeap::new(),
        }
    }

    pub fn push(&mut self, offset: u64, position: DefragPosition) {
        self.queue.push(DefragTask { offset, position, });
    }
}

#[derive(Clone, Debug)]
struct DefragTask {
    offset: u64,
    position: DefragPosition,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DefragPosition {
    StartAndBlock {
        right_block_id: block::Id,
    },
    TwoBlocks {
        left_block_id: block::Id,
        right_block_id: block::Id,
    },
}

impl PartialEq for DefragTask {
    fn eq(&self, other: &DefragTask) -> bool {
        self.offset == other.offset
    }
}

impl Eq for DefragTask { }

impl PartialOrd for DefragTask {
    fn partial_cmp(&self, other: &DefragTask) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DefragTask {
    fn cmp(&self, other: &DefragTask) -> cmp::Ordering {
        other.offset.cmp(&self.offset)
    }
}
