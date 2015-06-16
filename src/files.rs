use std::sync::mpsc::{Receiver, RecvError};
use std::fs::{File, OpenOptions};
use std::io::{self, Write, Read};
use std::collections::VecDeque;
use std::collections::vec_map::{VecMap, Entry};
use std::mem;
use std::marker::PhantomData;
use std::os::unix::io::AsRawFd;

use mio::buf::{Buf, MutBuf, RingBuf};
use mio::Sender;
use nix::sys::uio::{pread, pwrite};

use byteorder::{WriteBytesExt, BigEndian};

use torrent_info::TorrentInfo;
use peer_protocol::{PieceData, BLOCK_SIZE, SEND_BUF_SIZE};
use types::{BlockFromDisk, BlockRequest, BlockInfo,
    ConnectionId, PieceReaderMessage};


struct RequestQueue {
    requests: VecDeque<BlockRequest>,
    temp_buf: VecDeque<BlockRequest>,
}

impl RequestQueue {
    fn new() -> RequestQueue {
        RequestQueue {
            requests: VecDeque::with_capacity(16),
            temp_buf: VecDeque::with_capacity(16)
        }
    }

    fn enqueue(&mut self, request: BlockRequest) {
        self.requests.push_back(request);
    }

    fn dequeue(&mut self) -> Option<BlockRequest> {
        self.requests.pop_front()
    }

    fn cancel_all_from_peer(&mut self, conn_id: ConnectionId) {
        self.temp_buf.extend(
            self.requests.drain().filter(|req| req.receiver.id != conn_id)
        );
        mem::swap(&mut self.temp_buf, &mut self.requests);
    }

    fn cancel_request(&mut self, conn_id: ConnectionId, info: &BlockInfo) {
        let pos = self.requests.iter().position(
            |r| r.receiver.id == conn_id && &r.block_info == info);
        if let Some(index) = pos {
            self.requests.remove(index);
        }
    }
}

struct R;
struct W;

struct HandleCache<'a, AccessModeType> {
    handles: VecMap<File>,
    torrent: &'a TorrentInfo,
    _marker: PhantomData<AccessModeType>
}

impl <'a, T> HandleCache<'a, T> {
    fn new(torrent: &'a TorrentInfo) -> HandleCache<'a, T> {
        HandleCache {
            handles: VecMap::with_capacity(torrent.files().len()),
            torrent: torrent,
            _marker: PhantomData
        }
    }

    fn get_with_mode(&mut self, file_index: usize, read: bool, write: bool)
            -> &mut File {
        match self.handles.entry(file_index) {
            Entry::Vacant(v_entry) => {
                let path = &self.torrent.files()[file_index].path;
                v_entry.insert(
                    OpenOptions::new().read(read).write(write)
                                .create(true).open(path).unwrap()
                )
            },
            Entry::Occupied(o_entry) => o_entry.into_mut()
        }
    }
}

trait HandleCacheTrait {
    fn get(&mut self, file_index: usize) -> &mut File;
}

impl <'a> HandleCacheTrait for HandleCache<'a, R> {
    fn get(&mut self, file_index: usize) -> &mut File {
        self.get_with_mode(file_index, true, false)
    }
}

impl <'a> HandleCacheTrait for HandleCache<'a, W> {
    fn get(&mut self, file_index: usize) -> &mut File {
        self.get_with_mode(file_index, false, true)
    }
}

// TODO: optimisation: reuse the buffers
/// write piece data out to disk
pub fn file_writer(torrent_info: &TorrentInfo,
                   chan: Receiver<PieceData>) {
    let mut handles: HandleCache<W> = HandleCache::new(torrent_info);
    for piece in chan {
        // TODO (optimisation): write the whole piece at once (using writev)
        info!("Disk writer chan got a piece!");
        for (index, block) in &piece.blocks {
            let block_offset = index as u32 * BLOCK_SIZE;
            let sections = torrent_info.map_block(
                piece.index,
                block_offset,
                Buf::remaining(block) as u32);
            let mut remaining_data = block.bytes();
            for section in sections {
                let handle = handles.get(section.file_index);
                pwrite(handle.as_raw_fd(),
                       &remaining_data[..section.length as usize],
                       section.offset as i64).unwrap();
                remaining_data = &remaining_data[section.length as usize..];
            }
        }
    }
}

struct FileReader<'a> {
    torrent: &'a TorrentInfo,
    msg_chan: Receiver<PieceReaderMessage>,
    event_loop_sender: Sender<BlockFromDisk>,
    handles: HandleCache<'a, R>,
    requests: RequestQueue
}


impl <'a> FileReader<'a> {
    fn new(torrent: &'a TorrentInfo, msgs: Receiver<PieceReaderMessage>,
            event_loop_sender: Sender<BlockFromDisk>)
            -> FileReader<'a> {
        FileReader {
            torrent: torrent,
            msg_chan: msgs,
            event_loop_sender: event_loop_sender,
            handles: HandleCache::new(torrent),
            requests: RequestQueue::new()
        }
    }

    fn run(&mut self) {
        loop {
            let msg = match self.msg_chan.recv() {
                Ok(msg) => msg,
                Err(RecvError{}) => return
            };
            self.handle_message(msg);
            while let Some(req) = self.requests.dequeue() {
                self.handle_request(&req);
                while let Ok(msg) = self.msg_chan.try_recv() {
                    self.handle_message(msg);
                }
            }
        }
    }

    fn handle_message(&mut self, msg: PieceReaderMessage) {
        use types::PieceReaderMessage::*;
        match msg {
            Request(req) =>
                self.requests.enqueue(req),
            CancelRequestsForConnection(conn_id) =>
                self.requests.cancel_all_from_peer(conn_id),
            CancelRequest(ref info, conn_id) =>
                self.requests.cancel_request(conn_id, info)
        }
    }

    fn handle_request(&mut self, request: &BlockRequest) {
        let block_info = &request.block_info;
        // TODO: reuse buffers
        // XXX: we can't just send the raw data back, we need to include the
            // headers (or at least leave space for them so we can
            // write them somewhere in peer_protocol
        let mut data = RingBuf::new(SEND_BUF_SIZE);
        write_message_header(&mut data, block_info);
        let mut cur_offset_in_piece = 0;
        for section in self.torrent.map_block(block_info.piece_index,
                                              block_info.offset,
                                              block_info.length) {
            let handle = self.handles.get(section.file_index);
            let buf = &mut data.mut_bytes()
                          [cur_offset_in_piece..
                           cur_offset_in_piece + section.length as usize];
            pread(handle.as_raw_fd(), buf, section.offset as i64).unwrap();
            cur_offset_in_piece += section.length as usize;
        }
        MutBuf::advance(&mut data, block_info.length as usize);
        let block = BlockFromDisk {
            info: *block_info,
            data: data,
            receiver: request.receiver
        };
        self.event_loop_sender.send(block).ok().unwrap();
        // TODO: maybe exit more gracefully
    }
}

fn write_message_header<W: io::Write>(writer: &mut W, info: &BlockInfo) {
    let msg_length = info.length + 1 + 4 + 4;
    writer.write_u32::<BigEndian>(msg_length).unwrap();
    writer.write_u8(7).unwrap();
    writer.write_u32::<BigEndian>(info.piece_index.0).unwrap();
    writer.write_u32::<BigEndian>(info.offset).unwrap();
}

pub fn run_file_reader(torrent_info: &TorrentInfo,
                   msgs: Receiver<PieceReaderMessage>,
                   event_loop_sender: Sender<BlockFromDisk>) {
    let mut reader = FileReader::new(torrent_info, msgs, event_loop_sender);
    reader.run();
}


#[cfg(test)]
mod tests {
    use torrent_info::{TorrentInfo, FileInfo};

    fn dummy_torrent() -> TorrentInfo {
        let dummy_file =
            FileInfo { path: From::from("foo.mp4"), length: 512 * 1024 * 1024 };
        let piece_size = 1024 * 256;
        let piece_count = dummy_file.length / piece_size;
        let piece_hashes = vec![0u8;piece_count as usize * 20];
        TorrentInfo::new("dummy_name".to_string(), piece_size as u32,
                         piece_hashes, vec![dummy_file], b"").unwrap()
    }

    #[test]
    fn test_handle_cache() {
        // TODO: do more than smoke testing
        use super::{HandleCache, HandleCacheTrait, W};
        use std::io::Write;
        use std::fs::remove_file;

        let torrent = dummy_torrent();
        let mut cache: HandleCache<W> = HandleCache::new(&torrent);
        let f = cache.get(0);
        f.write(b"foo").unwrap();
        remove_file(&torrent.files()[0].path);
    }

    /*
    #[test]
    fn test_file_writer() {
        use types::{BlockInfo, BlockFromPeer};
        use mio::buf::RingBuf;
        use std::io::Write;
        use std::sync::mpsc::channel;
        // TODO:
        let torrent = dummy_torrent();
        let torrent = &torrent;
        // create channel
        let (sender, receiver) = channel();
        // spawn file writer thread
        let writer_guard = ::std::thread::scoped(
            move || super::file_writer(torrent, receiver)
        );
        // create a fake block, send it over the channel
        let mut buf = RingBuf::new(1024 * 32);
        for _ in (0..512) {
            buf.write(&[8u8;32][..]).unwrap();
        }
        let block_info =
            BlockInfo {piece_index: 0, offset: 0, length: 1024 * 16};
        let block = BlockFromPeer::new(block_info, buf);
        //sender.send(block).unwrap();
        writer_guard.join()

        // wait a bit
        // check if file has been created and block data written
    }*/
}
