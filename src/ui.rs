use std::collections::VecDeque;
use std::default::Default;
use std::fmt::Write;

use peer_protocol::CommonInfo;
use types::Stats;

use rustbox::{self, RustBox, Event, Color, Key};
use std::time::Duration;

pub struct UI {
    rustbox: RustBox,
    last_stats: Stats,
    uploaded_history: VecDeque<u64>,
    downloaded_history: VecDeque<u64>,
    tmp_buf: String
}

impl UI {
    pub fn init(stats: Stats) -> UI {
        UI {
            rustbox: RustBox::init(Default::default()).unwrap(),
            last_stats: stats,
            uploaded_history: VecDeque::with_capacity(SMA_WINDOW_SIZE),
            downloaded_history: VecDeque::with_capacity(SMA_WINDOW_SIZE),
            tmp_buf: String::new()
        }
    }

    pub fn update(&mut self, common: &CommonInfo) -> bool {
        let stats = common.current_stats();
        let up_since_last_tick = stats.uploaded - self.last_stats.uploaded;
        let down_since_last_tick = stats.downloaded - self.last_stats.downloaded;
        let up_avg = simple_moving_average(&mut self.uploaded_history,
                                           up_since_last_tick);
        let down_avg = simple_moving_average(&mut self.downloaded_history,
                                             down_since_last_tick);
        self.last_stats = stats;

        self.rustbox.print(1, 1, rustbox::RB_BOLD, Color::Green, Color::Black,
                      "gearbox!");
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Torrent Name: {}",
               common.torrent.name()).unwrap();
        self.rustbox.print(1, 2, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Downloaded: {}", stats.downloaded).unwrap();
        self.rustbox.print(1, 3, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Uploaded: {}", stats.uploaded).unwrap();
        self.rustbox.print(1, 4, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Remaining: {:<12} B", stats.remaining).unwrap();
        self.rustbox.print(1, 5, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Downloading at: {:<10} B/s", down_avg).unwrap();
        self.rustbox.print(1, 6, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Uploading at: {:<10} B/s", up_avg).unwrap();
        self.rustbox.print(1, 7, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        self.rustbox.print(1, 8, rustbox::RB_BOLD, Color::Red, Color::Black,
                           "\nPress q to quit");

        self.rustbox.present();

        loop {
            match self.rustbox.peek_event(Duration::from_secs(0), false) {
                Ok(Event::NoEvent) => return false,
                Ok(Event::KeyEvent(key)) => {
                    if let Key::Char('q') = key {
                        return true;
                    }
                },
                Err(_) => panic!("termbox error"),
                _ => ()
            }
        }
    }
}

const SMA_WINDOW_SIZE: usize = 10;
fn simple_moving_average(history: &mut VecDeque<u64>, new: u64) -> u64 {
    if history.len() == SMA_WINDOW_SIZE {
        history.pop_front();
    }
    history.push_back(new);
    let total: u64 = history.iter().fold(0, |acc, &x| acc + x);
    total / history.len() as u64
}
