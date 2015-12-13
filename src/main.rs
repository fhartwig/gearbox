#![feature(iter_arith)]

extern crate bytes;
extern crate bit_vec;
extern crate vec_map;
extern crate url;
extern crate hyper;
extern crate crypto;
extern crate mio;
extern crate rand;
#[macro_use]
extern crate log;
extern crate env_logger;
extern crate byteorder;
extern crate rustbox;
extern crate time;
extern crate nix;

use std::fs::File;
use std::path::PathBuf;
use std::io::Read;
use std::sync::mpsc::channel;
use std::sync::Arc;

use bencode::{BValue, FromBValue};
use peer_protocol::{run_event_loop, write_handshake, HANDSHAKE_BYTES_LENGTH};
use tracker::Tracker;
use torrent_info::TorrentInfo;

mod bencode;
mod torrent_info;
mod tracker;
mod peer;
mod files;
mod peer_protocol;
mod types;
mod piece_set;
mod ui;

fn main() {
    env_logger::init().unwrap();
    let path = PathBuf::from(&std::env::args().nth(1).unwrap());
    let mut data = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data).unwrap();
    let (tracker, torrent_info) = parse_torrent_file(&data[..]).unwrap();
    println!("Info: {:?}", torrent_info);
    let mut peer_id = [0u8;20];
    for b in &mut peer_id {
        *b = rand::random();
    }

    let mut handshake_buf = [0;HANDSHAKE_BYTES_LENGTH];
    write_handshake(&mut handshake_buf, &peer_id, &torrent_info);

    let (disk_reader_request_sender, disk_reader_request_receiver) = channel();
    let (disk_writer_sender, disk_writer_receiver) = channel();

    let torrent_info_arc = Arc::new(torrent_info);
    let torrent_info_arc_clone = torrent_info_arc.clone();
    let writer_guard = ::std::thread::spawn(
        move || files::file_writer(&torrent_info_arc_clone,
                                   disk_writer_receiver)
    );

    let event_loop = mio::EventLoop::new().ok()
            .expect("Error creating event loop");
    let block_from_disk_sender = event_loop.channel();
    let torrent_info_arc_clone = torrent_info_arc.clone();
    let reader_guard = ::std::thread::spawn(
        move || files::run_file_reader(&torrent_info_arc_clone,
                                   disk_reader_request_receiver,
                                   block_from_disk_sender)
    );

    run_event_loop(event_loop, &torrent_info_arc, disk_writer_sender,
                   disk_reader_request_sender, &peer_id, tracker,
                   &handshake_buf);
    reader_guard.join().expect("reader thread panicked");
    writer_guard.join().expect("writer thread panicked");
}

fn parse_torrent_file(data: &[u8]) -> Option<(Tracker, TorrentInfo)> {
    let mut bvalue = BValue::parse(data).unwrap();
    let info = bvalue.get(b"info").and_then(FromBValue::from_bvalue);
    let tracker = bvalue.get(b"announce").and_then(FromBValue::from_bvalue);
    match (tracker, info) {
        (Ok(tracker), Ok(info)) => Some((tracker, info)),
        _ => None
    }
}
