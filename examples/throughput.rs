

extern crate snowstorm;
extern crate time;

use snowstorm::channel::*;

use std::thread;
use std::sync::{Barrier, Arc};
use time::precise_time_s;

const TOTAL_MESSAGES: usize = 1_000_000_000;
const TOTAL_SENDERS: usize = 12;

fn main() {
    let (src, recv) = channel();

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

    let start = Arc::new(Barrier::new(TOTAL_SENDERS+1));
    let end = Arc::new(Barrier::new(TOTAL_SENDERS+1));

    for _ in (0..TOTAL_SENDERS) {
        let mut r = recv.clone();
        let start = start.clone();
        let end = end.clone();
        thread::spawn(move || {
            start.wait();
            for _ in r.copy_iter(false) {}
            end.wait();
        });
    }

    start.wait();
    let start_time = precise_time_s();
    end.wait();
    let end_time = precise_time_s();

    println!("Receive {} messages in {} seconds, {} messages/s accross {} threads.",
        TOTAL_MESSAGES*TOTAL_SENDERS,
        end_time - start_time,
        (TOTAL_MESSAGES*TOTAL_SENDERS) as f64 / (end_time - start_time),
        TOTAL_SENDERS
    );

    drop(src);
    drop(recv);

    let (src, mut recv) = channel();
    let end = Arc::new(Barrier::new(2));

    let start_time = precise_time_s();
    for i in (0..TOTAL_SENDERS) {
        let mut s = src.clone();
        thread::spawn(move || {
            let max = TOTAL_MESSAGES / TOTAL_SENDERS;
            for _ in (0..max) {
                s.send(i as u8);
            }
        });
    }
    src.close();
    
    let e = end.clone();
    thread::spawn(move || {
        let mut count = 0;
        for _ in recv.copy_iter(true) {
            count += 1;
            if count == TOTAL_MESSAGES {
                break;
            }
        }
        e.wait();
    });

    end.wait();
    let end_time = precise_time_s();
    println!("Stream {} messages in {} seconds, {} messages/s across written, {} messages/s read.",
        TOTAL_MESSAGES,
        end_time - start_time,
        (TOTAL_MESSAGES) as f64 / (end_time - start_time),
        (TOTAL_MESSAGES) as f64 / (end_time - start_time)
    );
}