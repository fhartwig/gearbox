#![feature(collections, core, scoped)]
#![cfg_attr(test, feature(from_raw_os))]

extern crate url;
extern crate hyper;
extern crate crypto;
extern crate mio;
extern crate rand;
#[macro_use]
extern crate log;
extern crate env_logger;
extern crate byteorder;

#[cfg(test)]
extern crate nix;

use std::fs::File;
use std::path::PathBuf;
use std::io::Read;
use std::sync::mpsc::channel;

use peer_protocol::{run_event_loop, LISTENING_PORT};
use tracker::{Tracker, Event};
use torrent_info::TorrentInfo;

mod bencode;
mod torrent_info;
mod tracker;
mod peer;
mod files;
mod peer_protocol;
mod types;
mod piece_set;

fn main() {
    env_logger::init().unwrap();
    let path = PathBuf::from(&std::env::args().nth(1).unwrap());
    let mut data_vec = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut data_vec).unwrap();
    let data = data_vec.into_boxed_slice();
    let (tracker, torrent_info) = parse_torrent_file(data).unwrap();
    let torrent_info_ref = &torrent_info;
    println!("Info: {:?}", torrent_info);
    let mut peer_id = [0u8;20];
    for b in peer_id.iter_mut() {
        *b = rand::random();
    }
    let r = tracker.make_request(torrent_info.info_hash(),
        &peer_id, Some(Event::Started), 0, 0, 0, LISTENING_PORT).unwrap();
    let (disk_reader_request_sender, disk_reader_request_receiver) = channel();
    let (disk_writer_sender, disk_writer_receiver) = channel();
    let writer_guard = ::std::thread::scoped(
        move || files::file_writer(torrent_info_ref, disk_writer_receiver)
    );

    let event_loop = mio::EventLoop::new().ok()
            .expect("Error opening event loop");
    let block_from_disk_sender = event_loop.channel();
    let reader_guard = ::std::thread::scoped(
        move || files::run_file_reader(torrent_info_ref,
                                   disk_reader_request_receiver,
                                   block_from_disk_sender)
    );

    println!("r: {:?}", r);
    run_event_loop(event_loop,
                   &torrent_info,
                   disk_writer_sender,
                   disk_reader_request_sender,
                   &peer_id,
                   tracker,
                   &r);
    reader_guard.join();
    writer_guard.join();
}

// TODO: where should this live?
fn parse_torrent_file<'a>(data: Box<[u8]>) -> Option<(Tracker, TorrentInfo)> {
    let mut bvalue = bencode::parse_bvalue(&*data).unwrap();
    let (info, tracker) = match (bvalue.get(b"info"), bvalue.get(b"announce")) {
        (Ok(info_dict), Ok(tracker)) => (info_dict, tracker),
        _ => return None
    };
    match (bencode::FromBValue::from_bvalue(tracker), bencode::FromBValue::from_bvalue(info)) {
        (Ok(tracker_value), Ok(info_value)) => Some((tracker_value, info_value)),
        _ => None
    }
}
