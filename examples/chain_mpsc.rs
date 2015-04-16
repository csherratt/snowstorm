extern crate time;

use std::thread;
use std::sync::mpsc::*;
use time::precise_time_s;

const THREADS: usize = 16;
const FRAMES: usize = 10000;
const ITEMS: usize = 10000;

fn worker(mut input: Receiver<(f64, f64)>, mut output: Sender<(f64, f64)>) {
	for (k, v) in input.iter() {
		output.send((k, v + v));
	}
}

fn main() {
	let (mut tx, mut rx) = channel();
	let tx2 = tx.clone();
	for _ in 0..THREADS {
		let (t, r) = channel();
		let tx2 = t.clone();
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
	for (i, (k, v)) in rx.iter().enumerate() {
		if i > ITEMS * FRAMES {
			break;
		}
		tx.send((k, v + v));
	}
	let end = precise_time_s();
	let t = end - start;
	println!("Send {} messages in {} seconds {} million messages per second",
		THREADS * ITEMS * FRAMES,
		t,
		(THREADS * ITEMS * FRAMES) as f64 / (1e6 * t)
	);

}