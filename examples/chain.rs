extern crate snowstorm;
extern crate time;

use snowstorm::channel::*;
use std::thread;
use time::precise_time_s;

const THREADS: usize = 12;
const FRAMES: usize = 10;
const ITEMS: usize = 10000000;

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
	for _ in 0..THREADS {
		let (t, r) = channel();
		thread::spawn(move || {
			worker(rx, t);
		});
		rx = r;
	}

	for j in 0..ITEMS {
		let v = j as f64;
		tx.send((v, v));
	}

	let start = precise_time_s();
	for _ in 0..FRAMES {
		tx.next_frame();

		loop {
			match rx.recv() {
				Ok(&(s, _)) => {
					tx.send((s, s));
				}
				Err(ReceiverError::EndOfFrame) => {
					rx.next_frame();
					break;
				},
				Err(ReceiverError::ChannelClosed) => {
					return;
				}
			}
		}
	}
	let end = precise_time_s();
	let t = end - start;
	println!("Send {} messages in {} seconds {} million messages per second",
		THREADS * ITEMS * FRAMES,
		t,
		(THREADS * ITEMS * FRAMES) as f64 / (1e6 * t)
	);

}