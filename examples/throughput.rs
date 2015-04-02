

extern crate snowstorm;
extern crate time;

use snowstorm::channel::*;

use std::thread;
use std::sync::{Barrier, Arc};
use time::precise_time_s;

const TOTAL_MESSAGES: usize = 10_000_000_000;
const TOTAL_SENDERS: usize = 12;

fn main() {
    let (src, sink) = channel();

    let start = Arc::new(Barrier::new(TOTAL_SENDERS+1));
    let end = Arc::new(Barrier::new(TOTAL_SENDERS+1));

    for i in (0..TOTAL_SENDERS) {
        let mut s = src.clone();
        let start = start.clone();
        let end = end.clone();
        thread::spawn(move || {
            start.wait();
            let max = TOTAL_MESSAGES / TOTAL_SENDERS;
            for _ in (0..max) {
                s.send(i as u8);
            }
            end.wait();
        });
    }

    start.wait();
    let start_time = precise_time_s();
    end.wait();
    let end_time = precise_time_s();

    println!("Send {} messages in {} seconds, {} messages/s accross {} threads.",
        TOTAL_MESSAGES,
        end_time - start_time,
        TOTAL_MESSAGES as f64 / (end_time - start_time),
        TOTAL_SENDERS
    );

}