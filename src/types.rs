use mio::buf::{RingBuf};
use mio;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PieceIndex(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct BlockRequest {
    pub block_info: BlockInfo,
    pub receiver: BlockReceiver
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub piece_index: PieceIndex,
    pub offset: u32,
    pub length: u32
}

pub struct BlockFromDisk {
    pub receiver: BlockReceiver,
    pub info: BlockInfo,
    pub data: RingBuf
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockReceiver {
    pub id: ConnectionId,
    pub token: mio::Token
}

#[derive(Clone, Copy, Debug)]
pub enum PieceReaderMessage {
    Request(BlockRequest),
    CancelRequest(BlockInfo, ConnectionId),
    CancelRequestsForConnection(ConnectionId)
}
