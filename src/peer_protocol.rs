use std::{io, mem};
use std::collections::{VecDeque, HashMap};
use std::cmp::min;
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
use std::sync::mpsc::Sender;

use torrent_info::TorrentInfo;
use tracker::{self, Tracker, Event, TrackerRequest};
use peer::PeerInfo;
use types::{BlockInfo, BlockFromDisk, BlockFromPeer, BlockRequest, PieceIndex,
    PieceReaderMessage, ConnectionId, BlockReceiver, Stats};
use piece_set::PieceSet;
use ui::UI;

use bytes::{Buf, ByteBuf, MutByteBuf, MutBuf, RingBuf, SliceBuf, Take};
use mio::{self, EventLoop, EventSet, Token, PollOpt, TryRead, TryWrite};
use mio::tcp::{TcpStream, TcpListener};
use mio::util::Slab;

use bit_vec::BitVec;
use vec_map::VecMap;

use crypto::sha1::Sha1;
use crypto::digest::Digest;

use byteorder::{ReadBytesExt, WriteBytesExt, BigEndian};

struct HandshakingConnection<'a> {
    sock: TcpStream,
    recv_buf: Take<MutByteBuf>,
    send_buf: SliceBuf<'a>
}

impl <'a> HandshakingConnection<'a> {
    fn new(sock: TcpStream, handshake_bytes: &'a [u8])
            -> HandshakingConnection<'a> {
        debug!("In handshakingConn::new");
        HandshakingConnection {
            sock: sock,
            recv_buf: Take::new(
                ByteBuf::mut_with_capacity(HANDSHAKE_BYTES_LENGTH),
                HANDSHAKE_BYTES_LENGTH
            ),
            send_buf: SliceBuf::wrap(handshake_bytes),
        }
    }

    fn open(peer: &PeerInfo, handshake_bytes: &'a [u8])
            -> HandshakingConnection<'a> {
        debug!("In handshakingConn::open");
        let stream = TcpStream::connect(&SocketAddr::V4(peer.addr)).unwrap();
        HandshakingConnection::new(stream, handshake_bytes)
    }

    /// return Err(()) on io error
    fn read(&mut self) -> Result<(), ()> {
        debug!("In handshakingConn::read");
        match self.sock.try_read_buf(&mut self.recv_buf) {
            Ok(Some(bytes_read)) => {
                debug!("Read {} bytes", bytes_read);
                Ok(())
            },
            Ok(None) => unreachable!(),
            Err(e) => {
                info!("Error reading: {:?}", e);
                Err(())
            }
        }
    }

    /// return Err(()) on IO error
    fn write(&mut self) -> Result<(), ()> {
        debug!("In handshakingConn::write");
        match self.sock.try_write_buf(&mut self.send_buf) {
            Ok(None) => Ok(()), // we need to wait before sending more data
            Ok(Some(_written_bytes)) => Ok(()),
            Err(_) => Err(())
        }
    }

    /// check whether we have sent and received an entire handshake
    fn handshake_finished(&self) -> bool {
        !Buf::has_remaining(&self.send_buf) &&
            self.recv_buf.get_ref().bytes().len() == HANDSHAKE_BYTES_LENGTH
    }

    /// finish handshake, creating a new PeerConnection
    fn finish_handshake(self, torrent: &TorrentInfo)
            -> Result<TcpStream, HandshakeError> {
        try!(self.verify_handshake(torrent));
        info!("Yay! Finished handshake!");
        Ok(self.sock)
    }

    fn verify_handshake(&self, torrent: &TorrentInfo)
            -> Result<(), HandshakeError> {
        // precondition: recv_buf contains entire handshake worth of bytes
        let handshake = self.recv_buf.get_ref().bytes();
        let protocol_name = &handshake[..20];
        if protocol_name != b"\x13BitTorrent protocol" {
            return Err(HandshakeError::BadProtocolHeader);
        }
        // the next 8 bytes are extension flags
        
        let info_hash = &handshake[28..48];
        if torrent.info_hash() != info_hash {
            return Err(HandshakeError::BadInfoHash);
        }
        // the next 20 bytes are the peer's peer id (which we ignore for now)
        Ok(())
    }
}

#[derive(Debug)]
pub enum HandshakeError {
    BadProtocolHeader,
    BadInfoHash
}

// TODO: make this a bitfield?
#[derive(Clone, Copy, Debug)]
pub struct ConnectionState {
    we_interested: bool,
    peer_interested: bool,
    we_choked: bool,
    peer_choked: bool
}

impl ConnectionState {
    fn new() -> ConnectionState {
        ConnectionState {
            we_interested: false,
            peer_interested: false,
            we_choked: true,
            peer_choked: true
        }
    }
}

#[derive(Debug)]
pub struct PeerConnection {
    conn_state: ConnectionState,
    sock: TcpStream,
    recv_buf: RingBuf,
    send_buf: RingBuf,
    peers_pieces: PieceSet,
    // blocks that the peer has requested but we haven't requested from
        // disk reader thread yet
    peer_request_queue: VecDeque<BlockInfo>,
    // blocks that the peer requested and we have already sitting in memory
        // (the buffers include the message header)
    outgoing_blocks: VecDeque<RingBuf>,
    outgoing_msgs: VecDeque<PeerMsg>, // short messages waiting to be sent
    currently_downloading_piece: Option<CurrentPieceInfo>, // FIXME: rename this
    current_piece_blocks: Option<PieceData>,
    next_piece_blocks: Vec<BlockFromPeer>,
    conn_id: ConnectionId,
    // number of blocks requested by the peer that we still have to deliver
    blocks_requested_by_peer: u8,
    token: Token,
    maybe_writable: bool,
    outgoing_buf_bytes_sent: u32,
    outgoing_block_length: Option<u16>,
}

enum Connection<'a> {
    Peer(PeerConnection),
    Handshaking(HandshakingConnection<'a>)
}

impl <'a> Connection<'a> {
    fn read(&mut self, common: &mut CommonInfo) {
        match *self {
            Connection::Handshaking(ref mut conn) => {
                conn.read().unwrap();
                if conn.handshake_finished() {
                    common.handler_action =
                        Some(HandlerAction::FinishedHandshake);
                }
            },
            Connection::Peer(ref mut conn) => conn.read(common)
        }
    }

    fn write(&mut self, common: &mut CommonInfo) {
        match *self {
            Connection::Handshaking(ref mut conn) => {
                conn.write().unwrap();
                if conn.handshake_finished() {
                    common.handler_action =
                        Some(HandlerAction::FinishedHandshake);
                }
            },
            Connection::Peer(ref mut conn) => {
                conn.writable(common)
            }
        }
    }

    fn finish_handshake(self, torrent: &TorrentInfo, id: ConnectionId,
                        tok: Token) -> PeerConnection {
        if let Connection::Handshaking(conn) = self {
            let sock = conn.finish_handshake(torrent).unwrap();
            // FIXME: can't just unwrap() this
            PeerConnection::new(sock, torrent, id, tok)
        } else {
            panic!("Tried to repeatedly finish handshake")
        }
    }
}

#[derive(Debug)]
pub struct PieceData {
    pub index: PieceIndex,
    pub blocks: VecMap<Take<RingBuf>>,
    block_count: u16,
    blocks_received: u16
}

impl PieceData {
    fn new(index: PieceIndex, torrent: &TorrentInfo) -> PieceData {
        let piece_size = torrent.get_piece_length(index);
        // integer_division rounding up:
        let block_count = (piece_size + BLOCK_SIZE - 1) / BLOCK_SIZE;
        PieceData {
            index: index,
            blocks: VecMap::with_capacity(block_count as usize),
            block_count: block_count as u16,
            blocks_received: 0
        }
    }

    fn add_block(&mut self, block: BlockFromPeer) {
        debug_assert!(block.info.piece_index == self.index);
        let block_index = block.info.offset / BLOCK_SIZE;
        debug_assert!(self.blocks.get(block_index as usize).is_none());
        self.blocks_received += 1;
        self.blocks.insert(block_index as usize,
                           Take::new(block.data, block.info.length as usize));
    }

    fn is_complete(&self) -> bool {
        self.block_count == self.blocks_received
    }

    fn verify(&self, common: &mut CommonInfo) -> bool {
        debug_assert!(self.is_complete());
        let mut digest = [0;20];
        for block in self.blocks.values() {
            common.piece_hash.input(block.bytes());
        }
        common.piece_hash.result(&mut digest);
        common.piece_hash.reset();
        digest == common.torrent.get_piece_hash(self.index)
    }
}

#[derive(Debug, Clone, Copy)]
struct CurrentPieceInfo {
    index: PieceIndex,
    requested_up_to: u32,
    length: u32
}

impl CurrentPieceInfo {
    /// construct the next block request. Returns None if whole block has been
    /// requested already
    fn next_block(&mut self) -> Option<BlockInfo> {
        if self.requested_up_to >= self.length {
            return None;
        }
        let block_offset = self.requested_up_to;
        let block_length = min(self.length - block_offset, BLOCK_SIZE);
        self.requested_up_to += block_length;
        Some(
            BlockInfo {
                piece_index: self.index,
                offset: block_offset,
                length: block_length
            }
        )
    }
}

pub struct CommonInfo<'a> {
    pub torrent: &'a TorrentInfo,
    /// pieces we have successfully downloaded
    our_pieces: PieceSet,
    /// pieces that we don't have and that are not yet being downloaded
    pieces_to_download: PieceSet,
    /// channel for requests to disk reader thread
    piece_reader_chan: Sender<PieceReaderMessage>,
    /// channel to disk writer thread
    piece_writer_chan: Sender<PieceData>,
    /// blocks that have been requested from disk thread but not delivered yet
    pending_disk_blocks: VecDeque<(ConnectionId, BlockInfo)>,
    /// message from connection to event handler
    handler_action: Option<HandlerAction>,
    bytes_downloaded: u64,
    bytes_uploaded: u64,
    piece_hash: Sha1,
}

impl <'a> CommonInfo<'a> {
    pub fn new(torrent: &'a TorrentInfo, our_pieces: PieceSet,
               reader_chan: Sender<PieceReaderMessage>,
               writer_chan:  Sender<PieceData>)
               -> CommonInfo<'a> {
        let pieces_to_download = our_pieces.inverse();
        CommonInfo {
            our_pieces: our_pieces,
            pieces_to_download: pieces_to_download,
            piece_reader_chan: reader_chan,
            piece_writer_chan: writer_chan,
            torrent: torrent,
            pending_disk_blocks: VecDeque::with_capacity(16),
            handler_action: None,
            bytes_downloaded: 0,
            bytes_uploaded: 0,
            piece_hash: Sha1::new()
        }
    }

    pub fn current_stats(&self) -> Stats {
        Stats {
            uploaded: self.bytes_uploaded,
            downloaded: self.bytes_downloaded,
            remaining: self.torrent.bytes_left_to_download(&self.our_pieces)
        }
    }
}

pub const BLOCK_SIZE: u32 = 1 << 14;

//buf needs to fit a block (2^14B) + message overhead
const RECV_BUF_SIZE: usize = 1 << 15;
pub const SEND_BUF_SIZE: usize = 1 << 15;

pub const HANDSHAKE_BYTES_LENGTH: usize = 1 + 19 + 8 + 20 + 20;

const CONCURRENT_REQUESTS_PER_PEER: u8 = 8;

impl PeerConnection {
    fn new(sock: TcpStream, torrent: &TorrentInfo,
           id: ConnectionId, token: Token) -> PeerConnection {
        PeerConnection {
            conn_state: ConnectionState::new(),
            sock: sock,
            recv_buf: RingBuf::new(RECV_BUF_SIZE), // TODO: use recycled buffer
            send_buf: RingBuf::new(SEND_BUF_SIZE),
            peers_pieces: PieceSet::new_empty(torrent),
            peer_request_queue: VecDeque::new(),
            outgoing_blocks: VecDeque::new(),
            outgoing_msgs: VecDeque::new(),
            currently_downloading_piece: None,
            current_piece_blocks: None,
            next_piece_blocks: Vec::new(),
            conn_id: id,
            blocks_requested_by_peer: 0,
            token: token,
            maybe_writable: true,
            outgoing_buf_bytes_sent: 0,
            outgoing_block_length: None,
        }
    }

    fn read(&mut self, common: &mut CommonInfo) {
        let messages_complete = self.read_message();
        if messages_complete {
            // TODO: error handling
            self.handle_messages(common).unwrap();
        }
    }

    fn writable(&mut self, common: &mut CommonInfo) {
        self.maybe_writable = true;
        if Buf::has_remaining(&self.send_buf) ||
                !self.outgoing_msgs.is_empty() {
            self.write(common)
        }
    }

    fn write(&mut self, common: &mut CommonInfo) {
        debug!("In PeerConnection::write");

        if !self.maybe_writable {return}

        // FIXME: this is extremely hacky, we should check if the messages
            // we have actually fit in the buffer
        if MutBuf::remaining(&self.send_buf) > 512 {
            self.append_queued_messages();
        }
        
        debug!("Sendbuf remaining: {}, capacity: {}",
                Buf::remaining(&self.send_buf), self.send_buf.capacity());
        match self.sock.try_write_buf(&mut self.send_buf) {
            Ok(None) => self.maybe_writable = false,
            Ok(Some(written_bytes)) => {
                info!("Wrote {} bytes of messages", written_bytes);
                self.outgoing_buf_bytes_sent += written_bytes as u32;
                if let Some(block_length) = self.outgoing_block_length {
                    if written_bytes > block_length as usize {
                        self.outgoing_block_length = None;
                        // -13 to account for msg header
                        common.bytes_uploaded += (block_length - 13) as u64;
                    }
                }
            },
            Err(_) => panic!("Error when writing") // TODO
        }
        if !Buf::has_remaining(&self.send_buf)
                && self.outgoing_msgs.is_empty() {
            self.try_replace_send_buf();
        }
    }

    fn try_replace_send_buf(&mut self) {
        match self.outgoing_blocks.pop_back() {
            Some(block_buf) => {
                self.outgoing_buf_bytes_sent = 0;
                self.outgoing_block_length =
                    Some(Buf::remaining(&block_buf) as u16);
                // TODO: recycle current buffer
                self.send_buf = block_buf;
            },
            None => { // we have nothing left to send
                // TODO: we should reset the buffer (because otherwise
                    // we will eventually wrap)
            }
        }
    }

    fn send_queued_messages(&mut self, common: &mut CommonInfo) {
        debug!("In PeerConnection::send_queued_messages");
        self.append_queued_messages();
        if Buf::has_remaining(&self.send_buf) {
            self.write(common);
        }
    }

    /// writes all queued messages to the end of the current send buffer
    fn append_queued_messages(&mut self) {
        debug!("In PeerConnection::append_queued_messages");
        // TODO: it is possible (in theory, but shouldn't really happen in
            // practice) that the total length of the messages execeeds the
            // capacity of the buffer. we should handle that case
        for msg in self.outgoing_msgs.drain(..) {
            debug!("Serialising msg: {:?}", msg);
            msg.serialise(&mut self.send_buf).unwrap();
        }
    }

    /// reads available data from socket, returns bool indicating whether
    /// a whole message is available or not
    fn read_message(&mut self) -> bool {
        debug!("In PeerConnection::read_message");
        // FIXME: apparently, on some platforms read() will return
        // 0 (repeatedly) when the connection has been closed
        match self.sock.try_read_buf(&mut self.recv_buf) {
            Ok(Some(bytes_read)) => {
                info!("Read {} bytes", bytes_read);
                if Buf::remaining(&self.recv_buf) < 4 {
                    return false;
                }
                let msg_length = self.read_message_length() as usize;
                Buf::remaining(&self.recv_buf) >= msg_length + 4
            },
            Ok(None) => {debug!("read() returned EWOULDBLOCK"); false}
            e => panic!("Unexpected return value from socket read: {:?}", e)
        }
    }

    fn read_message_length(&mut self) -> u32 {
        self.recv_buf.bytes().read_u32::<BigEndian>().unwrap()
    }

    fn handle_messages(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        loop {
            let msg_length = {
                let mut buf = &self.recv_buf.bytes()[..];
                if buf.len() < 4 { break };
                buf.read_u32::<BigEndian>().unwrap() as usize
            };
            let msg_offset = 4; // message not including length header
            debug!("Message length: {}", msg_length);
            if msg_length + msg_offset > Buf::remaining(&self.recv_buf) {
                break;
            }
            if msg_length == 0 { // keep-alive
                Buf::advance(&mut self.recv_buf, 4);
                continue;
            }
            let msg_payload_length = msg_length - 1;
            let msg_type_byte = self.recv_buf.bytes()[4];
            debug!("Msg type byte: {}", msg_type_byte);
            // advance to start of message
            Buf::advance(&mut self.recv_buf, 5);

            let mut buffer_replaced = false;
            match msg_type_byte {
                0 => try!(self.handle_choke()),
                1 => try!(self.handle_unchoke(common)),
                2 => {self.conn_state.peer_interested = true},
                3 => {self.conn_state.peer_interested = false},
                4 => try!(self.handle_have(common)),
                5 => try!(self.handle_bitfield(msg_payload_length, common)),
                6 => try!(self.handle_request(common)),
                7 => {
                    try!(self.handle_incoming_block(msg_payload_length,
                                                    common));
                    buffer_replaced = true;
                },
                8 => self.cancel_block(msg_offset, common),
                n => info!("Unknown message type: {}", n)
            }
            if !buffer_replaced {
                Buf::advance(&mut self.recv_buf, msg_payload_length);
            }
        }
        self.send_queued_messages(common);
        Ok(())
    }

    fn handle_choke(&mut self) -> PeerMsgResult {
        if self.conn_state.we_choked {
            return Ok(());
        }
        self.conn_state.we_choked = true;
        Ok(()) // TODO
    }

    fn handle_unchoke(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        if !self.conn_state.we_choked {
            return Ok(());
        }
        self.conn_state.we_choked = false;
        if self.currently_downloading_piece.is_none() {
            self.pick_piece_and_start_downloading(None, common);
        } else {
            // TODO
        }
        Ok(())
    }

    fn handle_have(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        let piece_index = {
            (&self.recv_buf.bytes()[..]).read_u32::<BigEndian>().unwrap()
        };
        if piece_index >= common.torrent.piece_count() {
            return Err(MsgError::PieceIndexTooLarge);
        }
        let piece_index = PieceIndex::from(piece_index);
        self.peers_pieces.set_true(piece_index);
        self.new_piece_available(Some(piece_index), common);
        Ok(())
    }

    fn start_downloading_piece(&mut self, common: &CommonInfo, piece_index: PieceIndex) {
        self.currently_downloading_piece = Some(
            CurrentPieceInfo {
                index: piece_index,
                length: common.torrent.get_piece_length(piece_index),
                requested_up_to: 0
            }
        );

        for _ in 0..3 {
            let next_block = self.currently_downloading_piece.as_mut().unwrap()
                                 .next_block();
            match next_block {
                None => break,
                Some(block) => self.request_block(block)
            }
        }
    }

    /// updates our record of the peer's pieces from a BitField message
    fn handle_bitfield(&mut self, msg_length: usize, common: &mut CommonInfo)
                       -> PeerMsgResult {
        let bitfield_byte_length =
            ((common.torrent.piece_count() + 7) / 8) as usize;
        {
            let bitfield = &self.recv_buf.bytes()[..msg_length];
            if bitfield_byte_length != bitfield.len() {
                return Err(MsgError::BadBitFieldLength);
            }

            let mut bit_offset = 0;
            for &byte in bitfield {
                for bit_index in 0..8 {
                    let has_piece = (byte >> (7 - bit_index)) & 0x01 == 0x01;
                    if has_piece {
                        self.peers_pieces.set_true(
                            PieceIndex::from(bit_offset + bit_index as u32)
                        );
                    }
                }
                bit_offset += 8;
            }
        }
        self.new_piece_available(None, common);
        Ok(())
    }

    fn handle_request(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        let mut reader = &self.recv_buf.bytes()[..12];
        let piece_index = PieceIndex::from(reader.read_u32::<BigEndian>().unwrap());
        let offset = reader.read_u32::<BigEndian>().unwrap();
        let length = reader.read_u32::<BigEndian>().unwrap();

        if u32::from(piece_index) > common.torrent.piece_count() {
            return Err(MsgError::PieceIndexTooLarge);
        }
        if !common.our_pieces[piece_index] {
            return Err(MsgError::PieceNotAvailable);
        }
        let expected_length =
            common.torrent.get_piece_length(piece_index);
        if length > 2u32.pow(14) || length > expected_length {
            return Err(MsgError::RequestedBlockTooLong);
        }
        if offset as u64 + length as u64 > expected_length as u64 {
            return Err(MsgError::BadOffsetLengthCombination);
        }
        let block = BlockInfo {
            piece_index: piece_index,
            offset: offset,
            length: length
        };

        // XXX: if peer is not choked and we're not already waiting for enough
            // blocks from piece reader thread:
        if !self.conn_state.peer_choked
        && self.blocks_requested_by_peer < CONCURRENT_REQUESTS_PER_PEER {
            common.piece_reader_chan.send(PieceReaderMessage::Request(
                BlockRequest {
                    block_info: block,
                    receiver: BlockReceiver {
                        id: self.conn_id,
                        token: self.token
                    }
                }
            )).unwrap();
            common.pending_disk_blocks.push_back((self.conn_id, block));
        } else {
            self.peer_request_queue.push_back(block);
        }
        self.blocks_requested_by_peer += 1;
        Ok(())
    }

    // FIXME: check if we actually requested incoming block
    fn handle_incoming_block(&mut self, msg_length: usize,
                    common: &mut CommonInfo) -> PeerMsgResult {
        let mut old_buf = self.replace_buf(msg_length as u32);
        let block_info = {
            let mut reader: &[u8] = old_buf.bytes();
            BlockInfo {
                // TODO: check that block info is valid
                piece_index: PieceIndex::from(
                    reader.read_u32::<BigEndian>().unwrap()
                ),
                offset: reader.read_u32::<BigEndian>().unwrap(),
                length: msg_length as u32 - 8
            }
        };
        Buf::advance(&mut old_buf, 8); // skip message header
        let piece_complete = {
            if self.current_piece_blocks.is_none() {
                self.current_piece_blocks = Some(
                    PieceData::new(block_info.piece_index,
                                    &common.torrent)
                );
            }
            let current_piece_blocks =
                self.current_piece_blocks.as_mut().unwrap();
            let block = BlockFromPeer::new(block_info, old_buf);
            if block_info.piece_index == current_piece_blocks.index {
                current_piece_blocks.add_block(block);
            } else {
                self.next_piece_blocks.push(block);
            }
            common.bytes_downloaded += block_info.length as u64;
            current_piece_blocks.is_complete()
        };
        if piece_complete {
            try!(self.handle_completed_piece(common));
        }
        let o_next_block = self.currently_downloading_piece.as_mut()
                               .and_then(|c| c.next_block());
        match o_next_block {
            None => {
                // we have requested all blocks of this piece
                self.pick_piece_and_start_downloading(None, common);
                if self.currently_downloading_piece.is_none() {
                    self.stop_being_interested();
                }
            },
            Some(block) => self.request_block(block)
        }
        Ok(())
    }

    fn handle_completed_piece(&mut self, common: &mut CommonInfo)
                              -> PeerMsgResult {
        let new_current_piece_blocks =
                if let Some(block) = self.next_piece_blocks.pop() {
            let next_piece_blocks =
                mem::replace(&mut self.next_piece_blocks, Vec::new());
            let new_current_piece_index = block.info.piece_index;
            let mut new_current_piece_blocks =
                PieceData::new(new_current_piece_index, &common.torrent);
            new_current_piece_blocks.add_block(block);
            for block in next_piece_blocks {
                if block.info.piece_index == new_current_piece_index {
                    new_current_piece_blocks.add_block(block);
                } else {
                    self.next_piece_blocks.push(block);
                }
            }
            Some(new_current_piece_blocks)
        } else {
            None
        };
        // FIXME: handle the (admittedly unlikely) case where new_current_piece
        // is already complete (because for some reason our the peer only
        // sent the last piece of the current after sending all the blocks
        // of the next piece). Currently this would be a problem, since
        // we only check if a piece is complete after receiving a block.

        let finished_piece_blocks = mem::replace(&mut self.current_piece_blocks,
                                                 new_current_piece_blocks)
                                         .unwrap();
        let index = finished_piece_blocks.index;
        if !finished_piece_blocks.verify(common) {
            return Err(MsgError::BadPieceHash)
        }
        common.our_pieces.set_true(index);
        common.piece_writer_chan.send(finished_piece_blocks).unwrap();
        common.handler_action = Some(HandlerAction::FinishedPiece(index));
        Ok(())
    }

    fn cancel_block(&mut self, msg_offset: usize, common: &CommonInfo) {
        debug!("In PeerConnection::cancel_block");
        // parse piece index, offset, length from message
        let mut reader = &self.recv_buf.bytes()[msg_offset..msg_offset + 12];
        // TODO: validate BlockInfo fields
        let block_info = BlockInfo {
            piece_index: PieceIndex::from(
                reader.read_u32::<BigEndian>().unwrap()
            ),
            offset: reader.read_u32::<BigEndian>().unwrap(),
            length: reader.read_u32::<BigEndian>().unwrap()
        };
        common.piece_reader_chan.send(
            PieceReaderMessage::CancelRequest(block_info, self.conn_id)
        ).unwrap();
        
        // put it in some kind of data structure so when we
        // get the data from reader thread, we don't actually send it to
        // the peer. (maybe also tell reader thread not to read it, but that
        // is probably not too important)
    }

    /// replace the buffer we're currently using, copying data past offset
    /// into the replacement buffer. Returns the old buffer
    fn replace_buf(&mut self, offset: u32) -> RingBuf {
        // TODO: use recycled buffer if possible
        let mut new_buf = RingBuf::new(RECV_BUF_SIZE);
        new_buf.write_slice(&self.recv_buf.bytes()[offset as usize..]);
        mem::replace(&mut self.recv_buf, new_buf)
    }

    #[inline]
    fn enqueue_msg(&mut self, msg: PeerMsg) {
        self.outgoing_msgs.push_back(msg);
    }

    /*
    fn choke_peer(&mut self) {
        self.enqueue_msg(PeerMsg::Choke);
        self.conn_state.peer_choked = true;
    }

    fn unchoke_peer(&mut self) {
        self.enqueue_msg(PeerMsg::Unchoke);
        self.conn_state.peer_choked = false;
        // TODO: if the peer has requested pieces, start sending them
    }*/

    // TODO: it's probably worth breaking this into two functions, one that
        // we call when we get a bitfield and one that we call when we get
        // `Have`
    fn new_piece_available(&mut self, new_available: Option<PieceIndex>,
            common: &mut CommonInfo) {
        if self.conn_state.we_choked {
            if !self.conn_state.we_interested {
                let become_interested = match new_available {
                    Some(index) => common.pieces_to_download[index],
                    None => common.pieces_to_download.has_new_pieces(&self.peers_pieces)
                };
                if become_interested {self.become_interested()}
            }
        } else if self.currently_downloading_piece.is_none() {
            if let Some(piece_index) = self.pick_piece(new_available, common) {
                if !self.conn_state.we_interested {
                    self.become_interested();
                }
                if !self.conn_state.we_choked {
                    self.start_downloading_piece(common, piece_index);
                }
            }
        }
    }

    fn pick_piece(&mut self, new_available: Option<PieceIndex>,
                    common: &mut CommonInfo) -> Option<PieceIndex> {
        match new_available {
            Some(piece_index) if common.pieces_to_download[piece_index] =>
                Some(piece_index),
            Some(_) => None,
            None =>
                common.pieces_to_download.pick_piece_from(&self.peers_pieces)
        }
    }

    fn pick_piece_and_start_downloading(&mut self,
                                        new_available: Option<PieceIndex>,
                                        common: &mut CommonInfo) {
        let o_piece = match new_available {
            Some(piece_index) if common.pieces_to_download[piece_index] =>
                Some(piece_index),
            Some(_) => None,
            None =>
                common.pieces_to_download.pick_piece_from(&self.peers_pieces)
        };
        if let Some(piece_index) = o_piece {
            self.start_downloading_piece(common, piece_index);
        } else {
            self.currently_downloading_piece = None
        }
    }

    fn stop_being_interested(&mut self) {
        self.enqueue_msg(PeerMsg::NotInterested);
        self.conn_state.we_interested = false;
    }

    fn notify_have(&mut self, piece_index: PieceIndex) {
        self.enqueue_msg(PeerMsg::Have(piece_index));
    }

    fn request_block(&mut self, block: BlockInfo) {
        self.enqueue_msg(PeerMsg::Request(block));
    }

    fn become_interested(&mut self) {
        self.conn_state.we_interested = true;
        self.enqueue_msg(PeerMsg::Interested);
    }

    fn add_outgoing_block(&mut self, block: RingBuf) {
        self.outgoing_blocks.push_back(block);
        // XXX: if we weren't sending data before, start sending now
    }

    fn try_send_bitfield(&mut self, our_pieces: &PieceSet) {
        debug!("In PeerConnection::try_send_bitfield");
        if !our_pieces.is_empty() {
            let writer = &mut self.send_buf;
            // FIXME: argh, why can't we get an iterator over u8 out of
                // BitVec?
            let bytes = our_pieces.to_bytes();
            writer.write_u32::<BigEndian>(1 + bytes.len() as u32).unwrap();
            writer.write_u8(5).unwrap();
            for byte in &bytes {
                writer.write_u8(*byte).unwrap();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PeerMsg {
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(PieceIndex),
    Request(BlockInfo),
    // Cancel(BlockInfo)
}
#[derive(Debug)]
enum MsgError {
    PieceIndexTooLarge,
    PieceNotAvailable,
    RequestedBlockTooLong,
    BadOffsetLengthCombination,
    BadBitFieldLength,
    BadPieceHash
}

type PeerMsgResult = Result<(), MsgError>;

impl PeerMsg {
    fn serialise<W: io::Write>(&self, writer: &mut W) -> io::Result<()>{
        use self::PeerMsg::*;
        match *self {
            Choke => {
                try!(writer.write_u32::<BigEndian>(1));
                try!(writer.write_u8(0));
            },
            Unchoke => {
                try!(writer.write_u32::<BigEndian>(1));
                try!(writer.write_u8(1));
            },
            Interested => {
                try!(writer.write_u32::<BigEndian>(1));
                try!(writer.write_u8(2));
            },
            NotInterested => {
                try!(writer.write_u32::<BigEndian>(1));
                try!(writer.write_u8(3));
            },
            Have(index) => {
                try!(writer.write_u32::<BigEndian>(1 + 4));
                try!(writer.write_u8(4));
                try!(writer.write_u32::<BigEndian>(u32::from(index)));
            },
            Request(block_info) => {
                try!(writer.write_u32::<BigEndian>(1 + 4 + 4 + 4));
                try!(writer.write_u8(6));
                try!(writer.write_u32::<BigEndian>(block_info.piece_index.into()));
                try!(writer.write_u32::<BigEndian>(block_info.offset));
                try!(writer.write_u32::<BigEndian>(block_info.length));
            }/*,
            Cancel(ref block_info) => {
                try!(writer.write_be_u32(1 + 4 + 4 + 4));
                try!(writer.write_u8(8));
                try!(writer.write_be_u32(block_info.piece_index));
                try!(writer.write_be_u32(block_info.offset));
                try!(writer.write_be_u32(block_info.length));
            },*/
        }
        Ok(())
    }
}

type ConnTokenMapping = HashMap<ConnectionId, Token>;

/// actions that the connection handlers can instruct the event loop to do
#[derive(Clone, Copy, Debug)]
enum HandlerAction {
    FinishedPiece(PieceIndex),
    FinishedHandshake
}


pub struct PeerEventHandler<'a> {
    listening_sock: TcpListener,
    connections: Slab<Connection<'a>>,
    cur_conn_id: u32,
    common_info: CommonInfo<'a>,
    own_peer_id: &'a [u8;20],
    tracker: Tracker,
    ui: UI,
    closed_conns: Vec<Token>,
    connection_closed: BitVec,
    handshake_bytes: &'a [u8]
}

impl <'a> PeerEventHandler<'a> {
    fn new(sock: TcpListener, torrent: &'a TorrentInfo,
           piece_reader_chan: Sender<PieceReaderMessage>,
           block_writer_chan: Sender<PieceData>,
           peer_id: &'a[u8;20], tracker: Tracker,
           handshake_bytes: &'a [u8]) -> PeerEventHandler<'a> {
        let our_pieces = torrent.check_downloaded_pieces();
        let common = CommonInfo::new(torrent, our_pieces, piece_reader_chan,
                                     block_writer_chan);
        let init_stats = common.current_stats();
        PeerEventHandler {
            connections: Slab::new_starting_at(Token(2), 256),
            listening_sock: sock,
            common_info: common,
            own_peer_id: peer_id,
            cur_conn_id: 0,
            tracker: tracker,
            ui: UI::init(init_stats),
            closed_conns: Vec::new(),
            connection_closed: BitVec::from_elem(257, false),
            handshake_bytes: handshake_bytes
        }
    }

    fn try_accept(&mut self, event_loop: &mut PeerEventLoop) {
        while let Some((conn, _)) = self.listening_sock.accept().unwrap() {
            let peer_conn =
                HandshakingConnection::new(conn, &self.handshake_bytes);
            self.connections.insert_with(|tok| {
                event_loop.register(&peer_conn.sock, tok,
                                    EventSet::readable() | EventSet::writable(),
                                    PollOpt::edge()).unwrap();
                Connection::Handshaking(peer_conn)
            });
        }
    }

    fn next_conn_id(&mut self) -> ConnectionId {
        let conn_id = self.cur_conn_id;
        self.cur_conn_id += 1;
        ConnectionId::from(conn_id)
    }

    fn handle_finished_piece(&mut self, piece_index: PieceIndex) {
        for conn in &mut self.connections {
            if let &mut Connection::Peer(ref mut peer_conn) = conn {
                peer_conn.notify_have(piece_index);
            }
        }
        if self.common_info.our_pieces.is_complete() {
            self.finish_downloading()
        }
    }

    fn finish_downloading(&mut self) {
        self.make_tracker_request(Some(tracker::Event::Completed));
        // TODO:
            // close all connections that don't want any more pieces
    }

    // FIXME: should we really talk to the tracker in the main thread?
    fn make_tracker_request(&mut self, event: Option<tracker::Event>) {
        let common = &self.common_info;
        let bytes_remaining =
            common.torrent.bytes_left_to_download(&common.our_pieces);
        let request = TrackerRequest {
            info_hash: &self.common_info.torrent.info_hash(),
            peer_id: self.own_peer_id,
            event: event,
            uploaded: common.bytes_uploaded,
            downloaded: common.bytes_downloaded,
            left: bytes_remaining,
            port: LISTENING_PORT
        };
        self.tracker.make_request(&request).unwrap();
    }
}

impl <'a> mio::Handler for PeerEventHandler<'a> {
    type Timeout = ();
    type Message = BlockFromDisk;

    fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token,
             events: EventSet) {
        // ignore events after connection has been closed:
        if self.connection_closed[token.as_usize()] { return }

        if events.is_error() || events.is_hup() {
            self.connection_closed.set(token.as_usize(), true);
            self.closed_conns.push(token);
            return;
        }

        if events.is_readable() {
            match token {
                LISTENER_TOKEN => self.try_accept(event_loop),
                _ => self.connections[token].read(&mut self.common_info)
            }
        }

        if events.is_writable() {
            self.connections[token].write(&mut self.common_info)
        }


        // check messages from the connection
        if let Some(a) = self.common_info.handler_action {
            match a {
                HandlerAction::FinishedPiece(index) =>
                    self.handle_finished_piece(index),
                HandlerAction::FinishedHandshake => {
                    let conn_id = self.next_conn_id();
                    let PeerEventHandler{ref mut connections, ref mut common_info, ..} = *self;
                    connections.replace_with(token, |conn| {
                        let mut peer_conn =
                            conn.finish_handshake(&common_info.torrent,
                                                  conn_id, token);
                        peer_conn.try_send_bitfield(&common_info.our_pieces);
                        peer_conn.read(common_info);
        // FIXME: read shouldn't really be called from here, since we
            // don't handle e.g. the action queue here (it's fine for now
            // though, because we only use the action queue when we have
            // received an entire piece, which is clearly not going to happen
            // here)
                        Some(Connection::Peer(peer_conn))
                    }).unwrap();
                }
            }
            self.common_info.handler_action = None;
        }
    }

    // TODO: this should be rewritten for readability / clarity
    fn notify(&mut self, _event_loop: &mut PeerEventLoop, msg: BlockFromDisk) {
        if let Connection::Peer(ref mut conn) = self.connections[msg.receiver.token] {
            if msg.receiver.id != conn.conn_id {
                if let Some(&(receiver_id, _)) =
                        self.common_info.pending_disk_blocks.front() {
                    if receiver_id == msg.receiver.id {
                        self.common_info.pending_disk_blocks.pop_front();
                    }
                }
                return; // the connection that requested that block has been closed
            }
            let expected = (msg.receiver.id, msg.info);
            // otherwise, we are receiving a block that we tried to cancel
            if self.common_info.pending_disk_blocks.front() == Some(&expected) {
                conn.add_outgoing_block(msg.data);
                self.common_info.pending_disk_blocks.pop_front();
            }
        }
    }

    fn timeout(&mut self, event_loop: &mut PeerEventLoop, _: Self::Timeout) {
        let quit = self.ui.update(&self.common_info);
        if quit {
            event_loop.shutdown();
        }
        event_loop.timeout_ms((), 1_000).unwrap();
    }

    fn tick(&mut self, _event_loop: &mut EventLoop<Self>) {
        // close all connections that are marked as closed
        for &token in &self.closed_conns {
            self.connection_closed.set(token.as_usize(), false);
            if let Some(Connection::Peer(PeerConnection{conn_id, ..})) =
                    self.connections.remove(token) {
                self.common_info.piece_reader_chan.send(
                    PieceReaderMessage::CancelRequestsForConnection(conn_id)
                ).unwrap();
            }
        }
        self.closed_conns.clear();
    }
}

pub type PeerEventLoop<'a> = mio::EventLoop<PeerEventHandler<'a>>;

pub const INIT_PEER_CONN_LIMIT: usize = 40;
fn initiate_peer_connections<'a>(event_loop: &mut PeerEventLoop,
                                 peers: &[PeerInfo],
                                 connections: &mut Slab<Connection<'a>>,
                                 handshake_bytes: &'a [u8]) {
    for peer in peers.iter().take(INIT_PEER_CONN_LIMIT) {
        let conn = HandshakingConnection::open(peer, handshake_bytes);
        connections.insert_with(|tok| {
            event_loop.register(&conn.sock, tok,
                                EventSet::readable() | EventSet::writable(),
                                PollOpt::edge()
            ).unwrap();
            Connection::Handshaking(conn)
        });
    }
}

const LISTENER_TOKEN: Token = Token(0);
pub const LISTENING_PORT: u16 = 8765;

pub fn run_event_loop<'a>(mut event_loop: PeerEventLoop<'a>,
                      torrent: &'a TorrentInfo,
                      block_writer_chan: Sender<PieceData>,
                      piece_reader_chan: Sender<PieceReaderMessage>,
                      peer_id: &'a [u8;20],
                      tracker: Tracker,
                      handshake: &'a [u8]) {
    let listening_addr = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(0, 0, 0, 0), LISTENING_PORT));
    let listener = TcpListener::bind(&listening_addr).unwrap();
    let mut handler = PeerEventHandler::new(listener, torrent,
                                            piece_reader_chan,
                                            block_writer_chan, peer_id,
                                            tracker, &handshake);
    let bytes_left_to_download =
        torrent.bytes_left_to_download(&handler.common_info.our_pieces);
    let tracker_request = TrackerRequest {
        info_hash: torrent.info_hash(),
        peer_id: peer_id,
        event: Some(Event::Started),
        uploaded: 0,
        downloaded: 0,
        left: bytes_left_to_download,
        port: LISTENING_PORT
    };
    let peers = handler.tracker.make_request(&tracker_request).unwrap();
    event_loop.register(&handler.listening_sock, LISTENER_TOKEN,
                            EventSet::readable(), PollOpt::edge()).unwrap();
    initiate_peer_connections(&mut event_loop, &peers, &mut handler.connections,
                              &handshake);
    event_loop.timeout_ms((), 2_000).unwrap();
    event_loop.run(&mut handler).ok().expect("Error in event_loop.run");
}

pub fn write_handshake(buf: &mut [u8], peer_id: &[u8], torrent: &TorrentInfo) {
    use std::io::{Cursor, Write};
    let mut c = Cursor::new(&mut buf[..]);
    c.write(b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
    c.write(torrent.info_hash()).unwrap();
    c.write(peer_id).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream as StdTcpStream;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::sync::mpsc::{channel, Receiver};

    use piece_set::PieceSet;
    use torrent_info::{TorrentInfo, FileInfo};
    use types::*;
    use mio::tcp::{TcpStream, TcpListener};
    use mio::Token;

    // TODO: proper error handling, although it's not that important for tests
    fn get_socket_pair() -> (TcpStream, StdTcpStream) {
        use nix::sys::socket::{SockType, AddressFamily, SockFlag, socketpair};

        let (our_sock, peer_sock) =
            socketpair(AddressFamily::Unix, SockType::Stream, 0,
                       SockFlag::empty())
                      .expect("Oh noes, socketpair doesn't work");

        let our_stream = unsafe { FromRawFd::from_raw_fd(our_sock) };
        let peer_stream = unsafe { FromRawFd::from_raw_fd(peer_sock) };

        (our_stream, peer_stream)
    }

    fn dummy_torrent() -> TorrentInfo {
        let dummy_file =
            FileInfo { path: From::from("abcd"), length: 512 * 1024 * 1024 };
        let piece_size = 1024 * 256;
        let piece_count = dummy_file.length / piece_size;
        let piece_hashes = vec![0u8;piece_count as usize * 20];
        TorrentInfo::new("dummy_name".to_string(), piece_size as u32,
                         piece_hashes, vec![dummy_file], b"").unwrap()
    }

    fn mk_common_info<'a>(torrent: &'a TorrentInfo)
                      -> (CommonInfo<'a>,
                          Receiver<PieceReaderMessage>,
                          Receiver<PieceData>) {
        let (reader_msg_tx, reader_msg_rx) = channel();
        let (block_tx, block_rx) = channel();
        let our_pieces = PieceSet::new_empty(&torrent);

        let common = CommonInfo::new(torrent, our_pieces, reader_msg_tx,
                                     block_tx);
        (common, reader_msg_rx, block_rx)
    }


    fn mk_peer_conn(torrent: &TorrentInfo) -> (PeerConnection, StdTcpStream) {
        let (our_conn, peer_conn) = get_socket_pair();
        let p_conn = PeerConnection::new(our_conn, torrent,
                                         ConnectionId::from(1), Token(1234));
        (p_conn, peer_conn)
    }

    fn mk_peer_event_handler<'a> (torrent: &'a TorrentInfo,
                                  peer_id: &'a [u8; 20],
                                  handshake: &'a [u8])
        -> (super::PeerEventHandler<'a>,
            Receiver<PieceData>,
            Receiver<PieceReaderMessage>) {
        use super::PeerEventHandler;
        use std::net::ToSocketAddrs;
        use url::Url;
        use tracker::Tracker;

        let (piece_reader_sender, piece_reader_receiver) = channel();
        let (piece_writer_sender, piece_writer_receiver) = channel();
        let tracker_url = Url::parse("http://localhost:8080/announce").unwrap();
        let tracker = Tracker::Http(tracker_url);
        let socket_addr = ToSocketAddrs::to_socket_addrs("0.0.0.0:0")
                                        .unwrap().next().unwrap();
        let listener = TcpListener::bind(&socket_addr).unwrap();

        let handler = PeerEventHandler::new(listener, torrent,
                                            piece_reader_sender,
                                            piece_writer_sender, peer_id,
                                            tracker, handshake);
        (handler, piece_writer_receiver, piece_reader_receiver)
    }

    #[test]
    fn test_handshake() {
        let torrent = dummy_torrent();
        let (nb_conn, mut peer_conn) = get_socket_pair();
        let our_id = [42;20];
        let peer_id = [23;20];
        let mut handshake = [0;super::HANDSHAKE_BYTES_LENGTH];
        super::write_handshake(&mut handshake, &our_id, &torrent);
        let mut hs_conn = super::HandshakingConnection::new(nb_conn,
                                                            &handshake);
        assert!(!hs_conn.handshake_finished());
        let mut peer_handshake = Vec::new();
        peer_handshake.extend(
            b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00");
        peer_handshake.extend(torrent.info_hash());
        peer_handshake.extend(&peer_id);
        peer_conn.write_all(&peer_handshake).unwrap();

        hs_conn.read().expect("error reading from hs_conn");
        assert!(!hs_conn.handshake_finished());
        hs_conn.write().unwrap();
        assert!(hs_conn.handshake_finished());
        let mut buf = [0u8;68];
        let bytes = peer_conn.read(&mut buf).unwrap();
        println!("Read {} bytes", bytes);
        assert_eq!(&buf[..28],
                   b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(&buf[28..48], torrent.info_hash());
        assert_eq!(&buf[48..], &our_id);
    }

    #[test]
    fn test_bitfield_handling() {
        use byteorder::{WriteBytesExt, BigEndian};

        let torrent = dummy_torrent();
        let (mut peer_conn, mut other_end) = mk_peer_conn(&torrent);

        let mut peers_piece_set = PieceSet::new_empty(&torrent);
        let (mut common, _, _) = mk_common_info(&torrent);
        peers_piece_set.set_true(PieceIndex::from(3));
        peers_piece_set.set_true(PieceIndex::from(23));
        peers_piece_set.set_true(PieceIndex::from(42));

        let bit_vec = peers_piece_set.to_bytes();
        let mut msg = Vec::new();
        let msg_length = 1 + bit_vec.len();

        msg.extend(&[0;4]); // placeholder for message length
        msg.push(5); // header byte for bitfield
        msg.extend(&bit_vec[..]);
        {
            let mut writer = &mut msg[..];
            writer.write_u32::<BigEndian>(msg_length as u32).unwrap();
        }
        other_end.write_all(&msg).unwrap();
        peer_conn.read(&mut common);
        assert!(!peer_conn.peers_pieces[PieceIndex::from(2)]);
        assert!(!peer_conn.peers_pieces[PieceIndex::from(4)]);
        assert!(peer_conn.peers_pieces[PieceIndex::from(3)]);
        assert!(peer_conn.peers_pieces[PieceIndex::from(23)]);
        assert!(peer_conn.peers_pieces[PieceIndex::from(42)]);

        // we should be interested...
        assert!(peer_conn.conn_state.we_interested);
        // .. and should tell that (and nothing else) to the peer
        let mut buf = [0;32];
        let bytes_read = other_end.read(&mut buf).unwrap();
        assert_eq!(bytes_read, 5);
        assert_eq!(&buf[..5], [0, 0, 0, 1, 2]);
        // we shouldn't send requests, since we're choked
        assert!(peer_conn.outgoing_msgs.is_empty());
        // we also shouldn't have a currently downloading piece (same reason)
        assert!(peer_conn.currently_downloading_piece.is_none());
    }

    #[test]
    fn test_serialise_peer_msg() {
        use super::PeerMsg;
        use bytes::{Buf, RingBuf};

        let mut buf = RingBuf::new(super::SEND_BUF_SIZE);
        {
            let msg = PeerMsg::Interested;
            msg.serialise(&mut buf).unwrap();
            let msg = PeerMsg::Have(PieceIndex::from(0));
            msg.serialise(&mut buf).unwrap();
        }
        let written = Buf::bytes(&buf);
        assert_eq!(&written[..5], [0, 0, 0, 1, 2]);
        assert_eq!(&written[5..5+9], [0, 0, 0, 5, 4, 0, 0, 0, 0]);
    }

    #[test]
    fn test_multiple_packed_msgs() {
        use super::PeerMsg;

        let torrent = dummy_torrent();
        let (mut peer_conn, mut other_end) = mk_peer_conn(&torrent);
        let (mut common, _, _) = mk_common_info(&torrent);
        let mut outgoing_buf = Vec::new();
        PeerMsg::Have(PieceIndex::from(23)).serialise(&mut outgoing_buf).unwrap();
        other_end.write_all(&outgoing_buf).unwrap();
        peer_conn.read(&mut common);
        assert!(peer_conn.peers_pieces[PieceIndex::from(23)]);
        peer_conn.peers_pieces.set_false(PieceIndex::from(23));

        outgoing_buf.clear();
        PeerMsg::Interested.serialise(&mut outgoing_buf).unwrap();
        PeerMsg::Have(PieceIndex::from(42))
                 .serialise(&mut outgoing_buf).unwrap();
        other_end.write(&outgoing_buf).unwrap();
        peer_conn.read(&mut common);
        // make sure the first message hasn't been handled twice
        assert!(!peer_conn.peers_pieces[PieceIndex::from(23)]);
        assert!(peer_conn.peers_pieces[PieceIndex::from(42)]);
        assert!(peer_conn.conn_state.peer_interested);
    }

    /*
    #[test]
    fn test_event_handling() {
        use super::PeerEventLoop;

        let torrent = dummy_torrent();
        let peer_id = [23;20];
        let (mut handler, _, _) = mk_peer_event_handler(&torrent, &peer_id);
        let (mut peer_conn, mut other_end) = mk_peer_conn(&torrent);
        let mut outgoing_buf = Vec::new();
        PeerMsg::Unchoke.serialise(&mut outgoing_buf);
        PeerMsg::Have(PieceIndex(42)).serialise(&mut outgoing_buf);

        let mut event_loop: PeerEventLoop = mio::EventLoop::new().unwrap();
        handler.add_conn_to_open_conns(peer_conn, &mut event_loop);
        other_end.write(&outgoing_buf);
        event_loop.run_once(&mut handler).unwrap();
        // TODO: figure out why we need the first run_once
        event_loop.run_once(&mut handler).unwrap();
        {
            let peer_conn_ref = &handler.open_conns[Token(1024)];
            assert!(peer_conn_ref.peers_pieces[PieceIndex(42)]);
            assert!(!peer_conn_ref.conn_state.we_choked);
        }

        outgoing_buf.clear();
        PeerMsg::Have(PieceIndex(23)).serialise(&mut outgoing_buf);
        other_end.write(&outgoing_buf);
        event_loop.run_once(&mut handler).unwrap();
        {
            let peer_conn_ref = &handler.open_conns[Token(1024)];
            assert!(peer_conn_ref.peers_pieces[PieceIndex(23)]);
        }
        let mut incoming_buf = [0;256];
        let read_bytes = other_end.read(&mut incoming_buf).unwrap();
        println!("{:?}", &incoming_buf[..read_bytes]);
        assert_eq!(read_bytes, 0);
        //assert_eq!(incoming_buf, //Interested, Req(42, 0, 0));
    }*/
}
