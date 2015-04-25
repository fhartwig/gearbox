use mio::buf::{RingBuf, Buf};
use mio;

// TODO: make this a newtype
pub type PieceIndex = u32;

// TODO: make this a newtype
pub type ConnectionId = u32;

#[derive(Clone, Copy, Debug)]
pub struct BlockRequest {
    pub block_info: BlockInfo,
    pub receiver: BlockReceiver
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo { pub piece_index: u32, pub offset: u32, pub length: u32 }

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

pub struct BlockFromPeer {
    pub info: BlockInfo,
    pub data: RingBuf
}

impl BlockFromPeer {
    pub fn new(info: BlockInfo, data: RingBuf) -> BlockFromPeer {
        BlockFromPeer {
            info: info,
            data: data
        }
    }
}

impl BlockFromPeer {
    pub fn as_slice(&self) -> &[u8] {
        &self.data.bytes()[..self.info.length as usize]
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PieceReaderMessage {
    Request(BlockRequest),
    CancelRequest(BlockInfo, ConnectionId),
    CancelRequestsForConnection(ConnectionId)
}
