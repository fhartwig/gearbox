use std::default::Default;
use std::fmt::Write;

use peer_protocol::CommonInfo;
use types::Stats;

use rustbox::{RustBox, Event, Color, Key};
use rustbox;

pub struct UI {
    rustbox: RustBox,
    stats: Stats,
    s: String
}

impl UI {
    pub fn init(stats: Stats) -> UI {
        UI {
            rustbox: RustBox::init(Default::default()).unwrap(),
            stats: stats,
            s: String::new()
        }
    }

    pub fn update(&mut self, common: &CommonInfo) -> bool {
        let stats = common.current_stats();
        self.rustbox.print(1, 1, rustbox::RB_BOLD, Color::White, Color::Black,
                      "gearbox!");
        self.s.clear();
        write!(&mut self.s, "Downloaded: {}", stats.downloaded).unwrap();
        self.rustbox.print(1, 2, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.s);
        self.s.clear();
        write!(&mut self.s, "Uploaded: {}", stats.uploaded).unwrap();
        self.rustbox.print(1, 3, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.s);
        self.s.clear();
        write!(&mut self.s, "Remaining: {}", stats.remaining).unwrap();
        self.rustbox.print(1, 4, rustbox::RB_BOLD, Color::White, Color::Black,
                      &self.s);
        self.rustbox.present();

        loop {
            match self.rustbox.peek_event(0, false) {
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
