use std::io;
use std::collections::{VecDeque, HashMap};
use std::cmp::min;

use std::mem::swap;
use std::sync::mpsc::Sender;

use torrent_info::TorrentInfo;
use tracker;
use tracker::{Tracker, Event};
use peer::PeerInfo;
use types::{BlockInfo, BlockFromPeer, BlockFromDisk, BlockRequest, PieceIndex,
    PieceReaderMessage, ConnectionId, BlockReceiver};
use piece_set::PieceSet;

use mio;
use mio::{NonBlock, Token, PollOpt, Interest, TryRead, TryWrite};
use mio::buf::{Buf, ByteBuf, MutBuf, MutSliceBuf, RingBuf};
use mio::util::Slab;
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr, TcpListener, TcpStream};

use crypto::sha1::Sha1;
use crypto::digest::Digest;

use byteorder::{ReadBytesExt, WriteBytesExt, BigEndian};

struct HandshakingConnection {
    conn: NonBlock<TcpStream>,
    recv_buf: Vec<u8>,
    send_buf: ByteBuf,
    bytes_received: usize
}

impl HandshakingConnection {
    fn new(conn: NonBlock<TcpStream>, torrent: &TorrentInfo, own_id: &[u8])
            -> HandshakingConnection {
        debug!("In handshakingConn::new");
        let mut send_buf = ByteBuf::mut_with_capacity(HANDSHAKE_BYTES_LENGTH);
        // TODO: everything we write out here is completely static (within
            // the same torrent). Maybe we should create this buffer once
            // and then just do send_buf.write_all(handshake_buf)
        send_buf.write_slice(
            b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00");
        send_buf.write_slice(torrent.info_hash());
        send_buf.write_slice(own_id);
        HandshakingConnection {
            conn: conn,
            recv_buf: vec![0;HANDSHAKE_BYTES_LENGTH],
            send_buf: send_buf.flip(),
            bytes_received: 0
        }
    }

    fn open(peer: &PeerInfo, torrent: &TorrentInfo, own_id: &[u8])
            -> HandshakingConnection {
        debug!("In handshakingConn::open");
        let sock = mio::tcp::v4().unwrap(); // TODO: error handling
        let (stream, _) = sock.connect(&SocketAddr::V4(peer.addr)).unwrap(); // TODO: what is the _ for?
        HandshakingConnection::new(stream, torrent, own_id)
    }

    /// return Err(()) on error, boolean indicating whether or not we're done
        /// otherwise
    fn read(&mut self) -> Result<(), ()> {
        debug!("In handshakingConn::read");
        let read_result = {
            let mut slice_buf = MutSliceBuf::wrap(
                &mut self.recv_buf[self.bytes_received..
                                   HANDSHAKE_BYTES_LENGTH]);
            self.conn.read(&mut slice_buf)
        };
        match read_result {
            Ok(Some(bytes_read)) => {
                debug!("Read {} bytes", bytes_read);
                self.bytes_received += bytes_read;
            },
            Ok(None) => unreachable!(),
            Err(e) => {info!("Error reading: {:?}", e); return Err(())}
        }
        Ok(())
    }

    /// result like read()
    fn write(&mut self) -> Result<(), ()> {
        debug!("In handshakingConn::write");
        match self.conn.write(&mut self.send_buf) {
            Ok(None) => Ok(()), // we need to wait before sending more data
            Ok(Some(_written_bytes)) => Ok(()),
            Err(_) => Err(())
        }
    }

    fn handshake_finished(&self) -> bool {
        !Buf::has_remaining(&self.send_buf) &&
            self.bytes_received >= HANDSHAKE_BYTES_LENGTH
    }

    /// finish handshake, creating a new PeerConnection
    fn finish_handshake(self, torrent: &TorrentInfo)
            -> Result<NonBlock<TcpStream>, &str> {
        try!(HandshakingConnection::verify_handshake(&self.recv_buf, torrent));
        info!("Yay! Finished handshake!");
        Ok(self.conn)
    }

    fn verify_handshake(handshake: &[u8], torrent: &TorrentInfo)
            -> Result<(), &'static str> {
        // precondition: recv_buf contains entire handshake worth of bytes
        let protocol_name = &handshake[..20];
        if protocol_name != b"\x13BitTorrent protocol" {
            return Err("bad protocol header");
        }
        // the next 8 bytes are extension flags
        
        let info_hash = &handshake[28..48];
        if torrent.info_hash() != info_hash {
            return Err("bad info hash");
        }
        // the next 20 bytes are the peer's peer id
        Ok(())
    }
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

// TODO: maybe store a "writable" bit that is set to 0 on EWOULDBLOCK and
// to 1 when we get a writable notification from the os?
pub struct PeerConnection {
    conn_state: ConnectionState,
    conn: NonBlock<TcpStream>,
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
    currently_downloading_piece: Option<CurrentPieceInfo>,
    // the (incremental, current) sha1 hash of the piece
    current_piece_hash: Sha1,
    conn_id: ConnectionId,
    // number of blocks requested by the peer that we still have to deliver
    blocks_requested_by_peer: u8,
    token: Token,
    maybe_writable: bool,
    outgoing_buf_bytes_sent: u32,
    outgoing_block_length: Option<u16>
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

struct CommonInfo<'a> {
    torrent: &'a TorrentInfo,
    /// pieces we have successfully downloaded
    our_pieces: PieceSet,
    /// pieces that we don't have and that are not yet being downloaded
    pieces_to_download: PieceSet,
    /// channel for requests to disk reader thread
    piece_reader_chan: Sender<PieceReaderMessage>,
    /// channel to disk writer thread
    piece_writer_chan: Sender<BlockFromPeer>,
    /// blocks that have been requested from disk thread but not delivered yet
    pending_disk_blocks: VecDeque<(ConnectionId, BlockInfo)>,
    /// message from connection to event handler
    handler_action: Option<HandlerAction>,
    bytes_downloaded: u64,
    bytes_uploaded: u64
}

impl <'a> CommonInfo<'a> {
    pub fn new(torrent: &'a TorrentInfo, our_pieces: PieceSet,
               reader_chan: Sender<PieceReaderMessage>,
               writer_chan:  Sender<BlockFromPeer>)
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
        }
    }
}

const BLOCK_SIZE: u32 = 1 << 14;

//buf needs to fit a block (2^14B) + message overhead
const RECV_BUF_SIZE: usize = 1 << 15;
const SEND_BUF_SIZE: usize = 1 << 15;

const HANDSHAKE_BYTES_LENGTH: usize = 1 + 19 + 8 + 20 + 20;

const CONCURRENT_REQUESTS_PER_PEER: u8 = 8;

impl PeerConnection {
    fn new(peer_conn: NonBlock<TcpStream>, torrent: &TorrentInfo,
           id: ConnectionId) -> PeerConnection {
        PeerConnection {
            conn_state: ConnectionState::new(),
            conn: peer_conn,
            recv_buf: RingBuf::new(RECV_BUF_SIZE), // TODO: use recycled buffer
            send_buf: RingBuf::new(SEND_BUF_SIZE),
            peers_pieces: PieceSet::new_empty(torrent),
            current_piece_hash: Sha1::new(),
            peer_request_queue: VecDeque::new(),
            outgoing_blocks: VecDeque::new(),
            outgoing_msgs: VecDeque::new(),
            currently_downloading_piece: None,
            conn_id: id,
            blocks_requested_by_peer: 0,
            token: Token(0), // FIXME: i don't like that the creator of the
                                // connection has to remember to set this
            maybe_writable: true,
            outgoing_buf_bytes_sent: 0,
            outgoing_block_length: None
        }
    }

    fn read(&mut self, common: &mut CommonInfo) {
        let messages_complete = self.read_message();
        if messages_complete {
            // TODO: error handling
            self.handle_messages(common).unwrap();
        }
    }

    fn write(&mut self, common: &mut CommonInfo) {
        debug!("In PeerConnection::write");
        if !self.maybe_writable {return}

        // FIXME: this is extremely hacky, we should check if the messages
            // we have actually fit in the buffer
        if MutBuf::remaining(&self.send_buf) > 512 {
            self.append_queued_messages(common);
        }
        
        debug!("Sendbuf remaining: {}, capacity: {}",
                Buf::remaining(&self.send_buf), self.send_buf.capacity());
        match self.conn.write(&mut self.send_buf) {
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
        self.append_queued_messages(common);
        if Buf::has_remaining(&self.send_buf) {
            self.write(common);
        }
    }

    /// writes all queued messages to the end of the current send buffer
    fn append_queued_messages(&mut self, common: &CommonInfo) {
        debug!("In PeerConnection::append_queued_messages");
        // TODO: it is possible (in theory, but shouldn't really happen in
            // practice) that the total length of the messages execeeds the
            // capacity of the buffer. we should handle that case
        for msg in self.outgoing_msgs.drain() {
            debug!("Serialising msg: {:?}", msg);
            msg.serialise(&mut self.send_buf, common).unwrap();
        }
    }

    /// reads available data from socket, returns bool indicating whether
    /// a whole message is available or not
    fn read_message(&mut self) -> bool {
        debug!("In PeerConnection::read_message");
        match self.conn.read(&mut self.recv_buf) {
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
        // read (any) data into buf
        // read message length (first 32 bits in buf)
        // if the entire message is in buffer, call handle_message
        // otherwise, read more data into buffer
    }

    fn read_message_length(&mut self) -> u32 {
        debug!("In PeerConnection::read_message_length");
        self.recv_buf.bytes().read_u32::<BigEndian>().unwrap()
    }

    fn handle_messages(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        debug!("In PeerConnection::handle_messages");
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
                    self.handle_incoming_block(msg_payload_length, common);
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
        debug!("In PeerConnection::handle_choke");
        if self.conn_state.we_choked {
            return Ok(());
        }
        self.conn_state.we_choked = true;
        Ok(()) // TODO
    }

    fn handle_unchoke(&mut self, common: &mut CommonInfo) -> PeerMsgResult {
        debug!("In PeerConnection::handle_unchoke");
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
        debug!("In PeerConnection::handle_have");
        let piece_index = {
            (&self.recv_buf.bytes()[..]).read_u32::<BigEndian>().unwrap()
        };
        if piece_index >= common.torrent.piece_count() {
            return Err(MsgError::PieceIndexTooLarge);
        }
        let piece_index = PieceIndex(piece_index);
        self.peers_pieces.set_true(piece_index);
        self.new_piece_available(Some(piece_index), common);
        Ok(())
    }

    fn start_downloading_piece(&mut self, common: &CommonInfo, piece_index: PieceIndex) {
        debug!("In PeerConnection::start_downloading_piece");
        self.currently_downloading_piece = Some(
            CurrentPieceInfo {
                index: piece_index,
                length: common.torrent.get_piece_length(piece_index),
                requested_up_to: 0
            }
        );

        for _ in (0..3) {
            let next_block = self.currently_downloading_piece.as_mut().unwrap()
                                 .next_block();
            match next_block {
                None => break,
                Some(block) => self.request_block(block)
            }
        }
    }

    fn handle_bitfield(&mut self, msg_length: usize, common: &mut CommonInfo)
                       -> PeerMsgResult {
        debug!("In PeerConnection::handle_bitfield");
        let bitfield_byte_length =
            ((common.torrent.piece_count() + 7) / 8) as usize;
        {
            let bitfield = &self.recv_buf.bytes()[..msg_length];
            if bitfield_byte_length != bitfield.len() {
                return Err(MsgError::BadBitFieldLength);
            }
            let mut bit_offset = 0;
            // TODO: what is the take() for?
            for &byte in bitfield.iter().take(bitfield_byte_length) {
                for bit_index in (0..8) {
                    let has_piece = (byte >> (7 - bit_index)) & 0x01 == 0x01;
                    if has_piece {
                        self.peers_pieces.set_true(
                            PieceIndex(bit_offset + bit_index as u32)
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
        debug!("In PeerConnection::handle_request");
        let mut reader = &self.recv_buf.bytes()[..12];
        let piece_index = reader.read_u32::<BigEndian>().unwrap();
        let offset = reader.read_u32::<BigEndian>().unwrap();
        let length = reader.read_u32::<BigEndian>().unwrap();

        if piece_index > common.torrent.piece_count() {
            return Err(MsgError::PieceIndexTooLarge);
        }
        if !common.our_pieces[PieceIndex(piece_index)] {
            return Err(MsgError::PieceNotAvailable);
        }
        let expected_length =
            common.torrent.get_piece_length(PieceIndex(piece_index));
        if length > 2u32.pow(14) || length > expected_length {
            return Err(MsgError::RequestedBlockTooLong);
        }
        if offset as u64 + length as u64 > expected_length as u64 {
            return Err(MsgError::BadOffsetLengthCombination);
        }
        let block = BlockInfo {
            piece_index: PieceIndex(piece_index),
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

    // TODO: check that the block we received is the one we expected
    fn handle_incoming_block(&mut self, msg_length: usize,
                    common: &mut CommonInfo) {
        debug!("In PeerConnection::handle_incoming_block");
        let mut old_buf = self.replace_buf(msg_length as u32);
        let block_info = {
            let mut reader: &[u8] = old_buf.bytes();
            BlockInfo {
                // TODO: check that block info is valid
                piece_index: PieceIndex(reader.read_u32::<BigEndian>().unwrap()),
                offset: reader.read_u32::<BigEndian>().unwrap(),
                length: msg_length as u32 - 8
            }
        };
        Buf::advance(&mut old_buf, 8); // skip message header
        {
            let block_data = &old_buf.bytes()[..block_info.length as usize];
            self.current_piece_hash.input(block_data);
        }
        // send buffer with block info to writer thread
        let block_for_writer = BlockFromPeer::new(block_info, old_buf);
        common.piece_writer_chan.send(block_for_writer).unwrap();
        let o_next_block =
            self.currently_downloading_piece.as_mut().unwrap().next_block();
        match o_next_block {
            None => {
                // we finished downloading this piece
                {
                    let cur_piece_ref =
                        self.currently_downloading_piece.as_ref().unwrap();
                    common.bytes_downloaded += cur_piece_ref.length as u64;
                    common.our_pieces.set_true(cur_piece_ref.index);
                    common.handler_action =
                        Some(HandlerAction::FinishedPiece(cur_piece_ref.index));
                }
                self.pick_piece_and_start_downloading(None, common);
                if self.currently_downloading_piece.is_none() {
                    self.stop_being_interested();
                }
            },
            Some(block) => self.request_block(block)
        }
    }

    fn cancel_block(&mut self, msg_offset: usize, common: &CommonInfo) {
        debug!("In PeerConnection::cancel_block");
        // parse piece index, offset, length from message
        let mut reader = &self.recv_buf.bytes()[msg_offset..msg_offset + 12];
        // TODO: validate BlockInfo fields
        let block_info = BlockInfo {
            piece_index: PieceIndex(reader.read_u32::<BigEndian>().unwrap()),
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
        debug!("In PeerConnection::replace_buf");
        // TODO: use recycled buffer if possible
        let mut new_buf = RingBuf::new(RECV_BUF_SIZE);
        {
            debug!("offset: {}, len: {}", offset, &self.recv_buf.bytes().len());
            let remaining_data = &self.recv_buf.bytes()[offset as usize..];
            new_buf.write_slice(remaining_data);
        }
        swap(&mut new_buf, &mut self.recv_buf);
        new_buf // this is actually the old buf
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

    fn new_piece_available(&mut self, new_available: Option<PieceIndex>,
            common: &mut CommonInfo) {
        if self.currently_downloading_piece.is_none() &&
                !self.conn_state.we_choked {
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
        debug!("In PeerConnection::pick_piece_and_start_downloading");
        let o_piece = match new_available {
            Some(piece_index) if common.pieces_to_download[piece_index] =>
                Some(piece_index),
            Some(_) => None,
            None =>
                common.pieces_to_download.pick_piece_from(&self.peers_pieces)
        };
        if let Some(piece_index) = o_piece {
            self.start_downloading_piece(common, piece_index);
        }
    }

    fn stop_being_interested(&mut self) {
        debug!("In PeerConnection::stop_being_interested");
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
            self.enqueue_msg(PeerMsg::BitField);
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
    BitField,
    Request(BlockInfo),
    // Piece(UploadBlock), // vec?
    // Cancel(BlockInfo)
}
#[derive(Debug)]
enum MsgError {
    PieceIndexTooLarge,
    PieceNotAvailable,
    RequestedBlockTooLong,
    BadOffsetLengthCombination,
    BadBitFieldLength
}

type PeerMsgResult = Result<(), MsgError>;

impl PeerMsg {
    fn serialise<W: io::Write>(&self, writer: &mut W, common: &CommonInfo)
                 -> io::Result<()>{
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
                try!(writer.write_u32::<BigEndian>(index.0));
            },
            BitField => {
                let bytes = common.our_pieces.to_bytes();
                try!(writer.write_u32::<BigEndian>(1 + bytes.len() as u32));
                try!(writer.write_u8(5));
                for byte in bytes.iter() {
                    try!(writer.write_u8(*byte));
                }
            },
            Request(block_info) => {
                try!(writer.write_u32::<BigEndian>(1 + 4 + 4 + 4));
                try!(writer.write_u8(6));
                try!(writer.write_u32::<BigEndian>(block_info.piece_index.0));
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
    FinishedPiece(PieceIndex)
}


struct PeerEventHandler<'a> {
    listening_sock: NonBlock<TcpListener>,
    open_conns: Slab<PeerConnection>,
    conn_nursery: Slab<HandshakingConnection>,
    cur_conn_id: u32,
    common_info: CommonInfo<'a>,
    own_peer_id: &'a [u8;20],
    tracker: Tracker
}

impl <'a>PeerEventHandler<'a> {
    fn new(sock: NonBlock<TcpListener>, torrent: &'a TorrentInfo,
           piece_reader_chan: Sender<PieceReaderMessage>,
           block_writer_chan: Sender<BlockFromPeer>,
           peer_id: &'a[u8;20], tracker: Tracker) -> PeerEventHandler<'a> {
        let our_pieces = torrent.check_downloaded_pieces();
        info!("downloaded_pieces: {:?}", our_pieces);
        PeerEventHandler {
            open_conns: Slab::new_starting_at(Token(1024), 128),
            conn_nursery: Slab::new_starting_at(Token(2), 128),
            listening_sock: sock,
            common_info: CommonInfo::new(torrent, our_pieces, piece_reader_chan,
                                         block_writer_chan),
            own_peer_id: peer_id,
            cur_conn_id: 0,
            tracker: tracker
        }
            // TODO: probably want to start at Token(2) (0 is listening for
            // new connections, 1 regularly talks to the tracker) (or maybe
            // we put the tracker communication into its own thread. there
            // isn't really any good reason to have it in the event loop,
            // except maybe to avoid concurrency issues)
    }

    fn try_accept(&mut self, event_loop: &mut PeerEventLoop) {
        let conn = match self.listening_sock.accept().unwrap() {
            Some(connection) => connection,
            None => return
        };
        let peer_conn = HandshakingConnection::new(conn,
                                                   &self.common_info.torrent,
                                                   self.own_peer_id);
        // TODO: this fails when slab is full
        let tok = self.conn_nursery.insert(peer_conn).ok().unwrap();
        let conn_ref = &self.conn_nursery[tok].conn;
        event_loop.register_opt(conn_ref, tok,
                Interest::readable() | Interest::writable(),
                PollOpt::edge()
        ).unwrap();
    }

    fn migrate_conn_from_nursery(&mut self, token: Token,
                                event_loop: &mut PeerEventLoop) {
        info!("Migrating conn {:?} from nursery", token);
        let conn = self.conn_nursery.remove(token).unwrap();
        let conn_id = self.next_conn_id();
        let sock = conn.finish_handshake(&self.common_info.torrent).unwrap();
        let mut peer_conn = PeerConnection::new(sock, &self.common_info.torrent,
                                                conn_id);
        peer_conn.try_send_bitfield(&self.common_info.our_pieces);
        self.add_conn_to_open_conns(peer_conn, event_loop);
    }

    fn add_conn_to_open_conns(&mut self, conn: PeerConnection,
                              event_loop: &mut PeerEventLoop) {
        let tok = self.open_conns.insert(conn).ok().unwrap();
        debug!("Inserted into open_conns. Token: {:?}", tok);
        let peer_conn_ref = &mut self.open_conns[tok];
        peer_conn_ref.token = tok;
        event_loop.deregister(&peer_conn_ref.conn).unwrap();
        event_loop.register_opt(&peer_conn_ref.conn, tok,
                Interest::readable() | Interest::writable(),
                PollOpt::edge() // TODO: do we want one-shot?
        ).unwrap();
        peer_conn_ref.read(&mut self.common_info);
        // FIXME: read shouldn't really be called from here, since we
            // don't handle e.g. the action queue here
    }

    fn next_conn_id(&mut self) -> ConnectionId {
        let conn_id = self.cur_conn_id;
        self.cur_conn_id += 1;
        ConnectionId(conn_id)
    }

    fn handle_finished_piece(&mut self, piece_index: PieceIndex) {
        for conn in self.open_conns.iter_mut() {
            conn.notify_have(piece_index);
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
        self.tracker.make_request(&self.common_info.torrent.info_hash(),
                                  self.own_peer_id, event,
                                  common.bytes_uploaded,
                                  common.bytes_downloaded,
                                  bytes_remaining, LISTENING_PORT).unwrap();
    }

    fn close_connection(&mut self, token: Token) {
        if let Some(&PeerConnection{conn_id, ..}) = self.open_conns.get(token) {
            self.open_conns.remove(token);
            self.common_info.piece_reader_chan.send(
                PieceReaderMessage::CancelRequestsForConnection(conn_id)
            ).unwrap();
            // TODO: recycle recv_buf and send_buf
        }
    }
}

impl <'a> mio::Handler for PeerEventHandler<'a> {
    type Timeout = ();
    type Message = BlockFromDisk;

    fn readable(&mut self, event_loop: &mut PeerEventLoop, token: Token,
                hint: mio::ReadHint) {
        info!("Readable. Hint: {:?}, token: {:?}", hint, token);
        match token {
            LISTENER_TOKEN => { // accept new connection, handshake etc
                self.try_accept(event_loop);
            }
            Token(n) if n < 1024 => {
                if !self.conn_nursery.contains(token) {
                    // FIXME: this is only necessary because we can get
                        // events after a connection has been closed
                    return;
                }
                if hint.is_error() || hint.is_hup() {
                    info!("closing connection {:?}", token);
                    let c = self.conn_nursery.remove(token).expect("no such token");
                    event_loop.deregister(&c.conn).ok().expect("error deregistering");
                }
                else if hint.is_data() {
                    let is_finished = {
                        let mut conn = &mut self.conn_nursery[token];
                        conn.read().unwrap();
                        conn.handshake_finished()
                    };
                    if is_finished {
                        self.migrate_conn_from_nursery(token, event_loop)
                    }
                }

            },
            Token(_) => {
                if hint.is_hup() || hint.is_error() {
                    self.close_connection(token);
                } else if hint.is_data() {
                    self.open_conns[token].read(&mut self.common_info);
                    // check messages from the connection
                    if let Some(a) = self.common_info.handler_action {
                        match a {
                            HandlerAction::FinishedPiece(index) =>
                                self.handle_finished_piece(index)
                        }
                        self.common_info.handler_action = None;
                    }
                }
            }
        }
    }

    fn writable(&mut self, event_loop: &mut PeerEventLoop, token: Token) {
        info!("writable. token: {:?}", token);
        let Token(n) = token;
        if n < 1024 { // half-open connection
            if !self.conn_nursery.contains(token) {
                // FIXME: this is only necessary because we can get writable
                    // events after a connection has been closed
                return;
            }
            let is_finished = {
                let mut conn = &mut self.conn_nursery[token];
                conn.write().unwrap();
                conn.handshake_finished()
            };
            if is_finished {
                self.migrate_conn_from_nursery(token, event_loop)
            }
        } else { // post-handshake connection
            // TODO: we need to handle the following scenario better:
                // 1: readable notification with hup-hint
                // 2: writable notification for the same token
            if let Some(conn) = self.open_conns.get_mut(token) {
                conn.maybe_writable = true;
                if Buf::has_remaining(&conn.send_buf) ||
                        !conn.outgoing_msgs.is_empty() {
                    conn.write(&mut self.common_info)
                }
            }
        }
    }

    // TODO: this should be rewritten for readability / clarity
    fn notify(&mut self, _event_loop: &mut PeerEventLoop, msg: BlockFromDisk) {
        let conn = &mut self.open_conns[msg.receiver.token];
        if msg.receiver.id != conn.conn_id {
            if let Some(&(receiver_id, _)) =
                    self.common_info.pending_disk_blocks.front() {
                if receiver_id == msg.receiver.id {
                    self.common_info.pending_disk_blocks.pop_front();
                }
            }
            return; // the connection that request that block was closed
        }
         let expected = (msg.receiver.id, msg.info);
        // otherwise, we are receiving a block that we tried to cancel
        if self.common_info.pending_disk_blocks.front() == Some(&expected) {
            conn.add_outgoing_block(msg.data);
            self.common_info.pending_disk_blocks.pop_front();
        }
    }
}

pub type PeerEventLoop<'a> = mio::EventLoop<PeerEventHandler<'a>>;

pub const INIT_PEER_CONN_LIMIT: usize = 40;
fn initiate_peer_connections(event_loop: &mut PeerEventLoop,
                            peers: &[PeerInfo], torrent: &TorrentInfo,
                            own_peer_id: &[u8;20],
                            conn_nursery: &mut Slab<HandshakingConnection>) {
    for peer in peers.iter().take(INIT_PEER_CONN_LIMIT) {
        let conn = HandshakingConnection::open(peer, torrent, own_peer_id);
                                               
        // put token in slab
        // register connection with event loop
        let tok = conn_nursery.insert(conn).ok().unwrap();
        event_loop.register_opt(&conn_nursery[tok].conn, tok,
                Interest::readable() | Interest::writable(),
                PollOpt::edge()
        ).unwrap();
    }
}

const LISTENER_TOKEN: Token = Token(0);
pub const LISTENING_PORT: u16 = 8765;

pub fn run_event_loop<'a>(mut event_loop: PeerEventLoop<'a>,
                      torrent: &'a TorrentInfo,
                      block_writer_chan: Sender<BlockFromPeer>,
                      piece_reader_chan: Sender<PieceReaderMessage>,
                      peer_id: &'a [u8;20],
                      tracker: Tracker) {
    let listening_addr = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(0, 0, 0, 0), LISTENING_PORT));
    let sock = mio::tcp::v4().unwrap_or_else(|_| panic!("Error creating socket"));
    sock.bind(&listening_addr).unwrap();
    let listener = sock.listen(20).unwrap();
    // listen on socket for incoming connnection
    let mut handler = PeerEventHandler::new(listener, torrent,
                                            piece_reader_chan,
                                            block_writer_chan, peer_id,
                                            tracker);
    let bytes_left_to_download =
        torrent.bytes_left_to_download(&handler.common_info.our_pieces);
    let peers = handler.tracker.make_request(torrent.info_hash(), peer_id,
                                             Some(Event::Started), 0, 0,
                                             bytes_left_to_download,
                                             LISTENING_PORT)
                               .unwrap();
    event_loop.register_opt(&handler.listening_sock, LISTENER_TOKEN,
                            Interest::readable(), PollOpt::edge()).unwrap();
    initiate_peer_connections(&mut event_loop, &peers, torrent, peer_id,
                              &mut handler.conn_nursery);
    event_loop.run(&mut handler).ok().expect("Error in event_loop.run");
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{CommonInfo, PeerMsg};
    use std::net::{TcpStream, SocketAddr};
    use std::os::unix::io::FromRawFd;
    use std::io::{Read, Write};
    use std::sync::mpsc::{channel, Sender, Receiver};
    use std::str::FromStr;

    use piece_set::PieceSet;
    use torrent_info::{TorrentInfo, FileInfo};
    use types::{BlockFromPeer, PieceReaderMessage};
    use mio::{IntoNonBlock, NonBlock, Token};
    use mio;

/*
    // TODO: proper error handling, although it's not that important for tests
    fn get_socket_pair() -> (TcpStream, TcpStream) {
        use nix::sys::socket::{SockType, AddressFamily, SockFlag, socketpair};
        use mio::IntoNonBlock;

        let (our_sock, peer_sock) =
            match socketpair(AddressFamily::Unix, SockType::Stream,
                             0, SockFlag::empty()) {
                Ok(tup) => tup,
                Err(_) => panic!("oh noes, socketpair doesn't work")
            };

        let our_stream = unsafe {FromRawFd::from_raw_fd(our_sock)};
        let peer_stream = unsafe {FromRawFd::from_raw_fd(peer_sock)};

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
                          Receiver<BlockFromPeer>) {
        let (reader_msg_tx, reader_msg_rx) = channel();
        let (block_tx, block_rx) = channel();
        let our_pieces = PieceSet::new_empty(&torrent);

        let common = CommonInfo::new(torrent, our_pieces, reader_msg_tx,
                                     block_tx);
        (common, reader_msg_rx, block_rx)
    }


    fn mk_peer_conn(torrent: &TorrentInfo) -> (PeerConnection, TcpStream) {
        // TODO: sensible variable names
        let (our_conn, peer_conn) = get_socket_pair();
        let nb_our_conn = IntoNonBlock::into_non_block(our_conn).unwrap();
        let p_conn = PeerConnection::new(nb_our_conn, torrent, 1);
        (p_conn, peer_conn)
    }

    fn mk_peer_event_handler<'a> (torrent: &'a TorrentInfo,
                                  peer_id: &'a [u8; 20])
        -> (super::PeerEventHandler<'a>,
            Receiver<BlockFromPeer>,
            Receiver<PieceReaderMessage>) {
        use super::PeerEventHandler;
        use std::net::TcpListener;
        use url::Url;
        use tracker::Tracker;

        let (piece_reader_sender, piece_reader_receiver) = channel();
        let (piece_writer_sender, piece_writer_receiver) = channel();
        let tracker_url = Url::parse("http://localhost:8080/announce").unwrap();
        let tracker = Tracker::new(tracker_url);
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let listener = IntoNonBlock::into_non_block(listener).unwrap();

        let handler = PeerEventHandler::new(listener, torrent,
                                            piece_reader_sender,
                                            piece_writer_sender, peer_id,
                                            tracker);
        (handler, piece_writer_receiver, piece_reader_receiver)
    }

    #[test]
    fn test_handshake() {
        let torrent = dummy_torrent();
        let (our_conn, mut peer_conn) = get_socket_pair();
        let our_id = [42;20];
        let peer_id = [23;20];
        let mut nb_conn = IntoNonBlock::into_non_block(our_conn).unwrap();
        let mut hs_conn =
            super::HandshakingConnection::new(nb_conn, &torrent, &our_id);
        assert!(!hs_conn.handshake_finished());
        let mut peer_handshake = Vec::new();
        peer_handshake.push_all(
            b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00");
        peer_handshake.push_all(&torrent.info_hash);
        peer_handshake.push_all(&peer_id);
        peer_conn.write_all(&peer_handshake);

        hs_conn.read();
        assert!(!hs_conn.handshake_finished());
        hs_conn.write().unwrap();
        assert!(hs_conn.handshake_finished());
        let mut buf = [0u8;68];
        let bytes = peer_conn.read(&mut buf).unwrap();
        println!("Read {} bytes", bytes);
        assert_eq!(&buf[..28],
                   b"\x13BitTorrent protocol\x00\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(&buf[28..48], &torrent.info_hash[..]);
        assert_eq!(&buf[48..], &our_id);
    }

    #[test]
    fn test_bitfield_handling() {
        use byteorder::{WriteBytesExt, BigEndian};
        use super::PeerMsg;

        let torrent = dummy_torrent();
        let (mut peer_conn, mut other_end) = mk_peer_conn(&torrent);

        let mut peers_piece_set = PieceSet::new_empty(&torrent);
        let (mut common, _, _) = mk_common_info(&torrent);
        peers_piece_set.set_true(3);
        peers_piece_set.set_true(23);
        peers_piece_set.set_true(42);

        let bit_vec = peers_piece_set.to_bytes();
        let mut n_buf = [0u8;4];
        let mut msg = Vec::new();
        let msg_length = 1 + bit_vec.len();

        msg.push_all(&[0;4]); // placeholder for message length
        msg.push(5); // header byte for bitfield
        msg.push_all(&bit_vec[..]);
        {
            let mut writer = &mut msg[..];
            writer.write_u32::<BigEndian>(msg_length as u32);
        }
        other_end.write_all(&msg);
        peer_conn.read(&mut common);
        assert!(!peer_conn.peers_pieces.get(2));
        assert!(!peer_conn.peers_pieces.get(4));
        assert!(peer_conn.peers_pieces.get(3));
        assert!(peer_conn.peers_pieces.get(23));
        assert!(peer_conn.peers_pieces.get(42));

        assert!(peer_conn.conn_state.we_interested);
        assert_eq!(peer_conn.outgoing_msgs.pop_front().unwrap(),
                   PeerMsg::Interested);
        // we shouldn't send requests, since we're choked
        assert!(peer_conn.outgoing_msgs.is_empty());
        // we also shouldn't have a currently downloading piece (same reason)
        assert!(peer_conn.currently_downloading_piece.is_none());
    }

    #[test]
    fn test_serialise_peer_msg() {
        use super::PeerMsg;
        use mio::buf::{Buf, RingBuf};

        let mut buf = RingBuf::new(super::SEND_BUF_SIZE);
        {
            let msg = PeerMsg::Interested;
            msg.serialise(&mut buf);
            let msg = PeerMsg::Have(0);
            msg.serialise(&mut buf);
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
        let mut piece_set = PieceSet::new_empty(&torrent);
        piece_set.set_true(23);
        let mut outgoing_buf = Vec::new();
        PeerMsg::BitField(piece_set).serialise(&mut outgoing_buf);
        other_end.write_all(&outgoing_buf);
        peer_conn.read(&mut common);
        assert!(peer_conn.peers_pieces.get(23));
        peer_conn.peers_pieces.set_false(23);

        outgoing_buf.clear();
        PeerMsg::Interested.serialise(&mut outgoing_buf);
        PeerMsg::Have(42).serialise(&mut outgoing_buf);
        other_end.write(&outgoing_buf);
        peer_conn.read(&mut common);
        // make sure the first message hasn't been handled twice
        assert!(!peer_conn.peers_pieces.get(23));
        assert!(peer_conn.peers_pieces.get(42));
        assert!(peer_conn.conn_state.peer_interested);
    }

    #[test]
    fn test_event_handling() {
        use super::PeerEventLoop;

        let torrent = dummy_torrent();
        let peer_id = [23;20];
        let (mut handler, _, _) = mk_peer_event_handler(&torrent, &peer_id);
        let (mut peer_conn, mut other_end) = mk_peer_conn(&torrent);
        let mut outgoing_buf = Vec::new();
        PeerMsg::Unchoke.serialise(&mut outgoing_buf);
        PeerMsg::Have(42).serialise(&mut outgoing_buf);

        let mut event_loop: PeerEventLoop = mio::EventLoop::new().unwrap();
        handler.add_conn_to_open_conns(peer_conn, &mut event_loop);
        other_end.write(&outgoing_buf);
        event_loop.run_once(&mut handler).unwrap();
        // TODO: figure out why we need the first run_once
        event_loop.run_once(&mut handler).unwrap();
        {
            let peer_conn_ref = &handler.open_conns[Token(1024)];
            assert!(peer_conn_ref.peers_pieces.get(42));
            assert!(!peer_conn_ref.conn_state.we_choked);
        }

        outgoing_buf.clear();
        PeerMsg::Have(23).serialise(&mut outgoing_buf);
        other_end.write(&outgoing_buf);
        event_loop.run_once(&mut handler).unwrap();
        {
            let peer_conn_ref = &handler.open_conns[Token(1024)];
            assert!(peer_conn_ref.peers_pieces.get(23));
        }
        let mut incoming_buf = [0;256];
        let read_bytes = other_end.read(&mut incoming_buf).unwrap();
        println!("{:?}", &incoming_buf[..read_bytes]);
        assert_eq!(read_bytes, 0);
        //assert_eq!(incoming_buf, //Interested, Req(42, 0, 0));
    }
*/
}
