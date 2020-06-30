use futures::{
    channel::{
        oneshot,
    },
};

use super::{
    block,
};

pub enum Request {
    LendBlock(RequestLendBlock),
    RepayBlock(RequestRepayBlock),
    WriteBlock(RequestWriteBlock),
    ReadBlock(RequestReadBlock),
    DeleteBlock(RequestDeleteBlock),
}

pub struct RequestLendBlock {
    pub reply_tx: oneshot::Sender<block::BytesMut>,
}

pub struct RequestRepayBlock {
    pub block_bytes: block::Bytes,
}

#[derive(Debug)]
pub enum RequestWriteBlockError {
    NoSpaceLeft,
}

pub struct RequestWriteBlock {
    pub block_bytes: block::Bytes,
    pub reply_tx: oneshot::Sender<Result<block::Id, RequestWriteBlockError>>,
}

pub struct RequestReadBlock {
    pub block_id: block::Id,
    pub reply_tx: oneshot::Sender<block::BytesMut>,
}

pub struct RequestDeleteBlock {
    pub block_id: block::Id,
    pub reply_tx: oneshot::Sender<Deleted>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Deleted;