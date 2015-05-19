use std::collections::VecDeque;
use std::default::Default;
use std::fmt::Write;

use peer_protocol::CommonInfo;
use types::Stats;

use rustbox::{self, RustBox, Event, Color, Key};
use time::Duration;

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

        self.rustbox.print(1, 1, rustbox::RB_BOLD, Color::White, Color::Black,
                      "gearbox!");
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Downloaded: {}", stats.downloaded).unwrap();
        self.rustbox.print(1, 2, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Uploaded: {}", stats.uploaded).unwrap();
        self.rustbox.print(1, 3, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Remaining: {:<12} B", stats.remaining).unwrap();
        self.rustbox.print(1, 4, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Downloading at: {:<10} B/s", down_avg).unwrap();
        self.rustbox.print(1, 5, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);
        self.tmp_buf.clear();
        write!(&mut self.tmp_buf, "Uploading at: {:<10} B/s", up_avg).unwrap();
        self.rustbox.print(1, 6, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.tmp_buf);

        self.rustbox.present();

        loop {
            match self.rustbox.peek_event(Duration::seconds(0), false) {
                Ok(Event::NoEvent) => return false,
                Ok(Event::KeyEvent(key)) => {
                    match key {
                        Some(Key::Char('q')) => return true,
                        _ => ()
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
    let total: u64 = history.iter().sum();
    let avg = total / history.len() as u64;
    avg
}
