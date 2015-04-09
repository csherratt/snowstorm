extern crate snowstorm;
extern crate time;

use snowstorm::channel::*;
use std::thread;
use time::precise_time_s;

fn worker(mut input: Receiver<(f64, f64)>, mut output: Sender<(f64, f64)>) {
	loop {
		match input.recv() {
			Ok(&(k, v)) => {
				output.send((k, v + v));
			}
			Err(ReceiverError::EndOfFrame) => {
				input.next_frame();
				output.next_frame();
			}
			Err(ReceiverError::ChannelClosed) => {
				return;
			}
		}
	}
}

fn main() {
	let (mut tx, mut rx) = channel();	
	for _ in 0..100 {
		let (mut t, mut r) = channel();
		thread::spawn(move || {
			worker(rx, t);
		});
		rx = r;
	}

	for i in 0..1_000 {
		let start = precise_time_s();

		for j in 0..10_000 {
			let v = j as f64 * i as f64;
			tx.send((v, v));
		}
		tx.next_frame();

		loop {
			match rx.recv() {
				Ok(&(s, o)) => {
					//println!("{} => {}", s, o);
				}
				Err(ReceiverError::EndOfFrame) => {
					rx.next_frame();
					let end = precise_time_s();
					println!("{} took {}s", i, end - start);
					break;
				},
				Err(ReceiverError::ChannelClosed) => {
					return;
				}
			}
		}
	}
}