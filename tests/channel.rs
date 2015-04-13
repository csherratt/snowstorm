extern crate snowstorm;

use std::thread;
use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};
use std::sync::atomic::*;
use snowstorm::channel::{channel, ReceiverError};

struct Canary(Arc<AtomicUsize>);

impl Drop for Canary {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn channel_send() {
    let (mut s, mut r) = channel();
    for i in (0..1000) {
        s.send(i);
    }

    s.flush();

    for i in (0..1000) {
        assert_eq!(r.try_recv(), Some(&i));
    }
}

#[test]
fn channel_send_multiple_recv() {
    let (mut s, mut r0) = channel();
    for i in (0..1000) {
        s.send(i);
    }

    s.flush();

    let mut r1 = r0.clone();
    for i in (0..1000) {
        assert_eq!(r0.try_recv(), Some(&i));
    }
    for i in (0..1000) {
        assert_eq!(r1.try_recv(), Some(&i));
    }
}

#[test]
fn channel_multiple_send() {
    let (mut s0, mut r) = channel();
    let mut s1 = s0.clone();

    for i in (0..1000) {
        s0.send(i);
    }
    s0.flush();

    for i in (1000..2000) {
        s1.send(i);
    }
    s1.flush();

    for i in (0..2000) {
        assert_eq!(r.try_recv(), Some(&i));
    }
}

#[test]
fn channel_drop() {
    let v = Arc::new(AtomicUsize::new(0));
    let (mut s, r) = channel();

    for i in (0..1000) {
        s.send(Canary(v.clone()));
        if i & 0xF == 8 {
            s.flush();
        }
    }

    assert_eq!(v.load(Ordering::SeqCst), 0);
    drop(s);
    drop(r);
    assert_eq!(v.load(Ordering::SeqCst), 1000);
}

#[test]
fn hundred_writers() {
    let (s0, mut r) = channel();

    let start = Arc::new(Barrier::new(100));
    let end = Arc::new(Barrier::new(100));

    for i in (0..100) {
        let mut s = s0.clone();
        let start = start.clone();
        let end = end.clone();
        thread::spawn(move || {
            start.wait();
            for j in (0..100) {
                s.send(i*100 + j);
            }
            end.wait();
        });
    }

    let mut expected: BTreeSet<i32> = (0..10_000).collect();
    while let Some(i) = r.try_recv() {
        assert_eq!(expected.remove(i), true);
    }
    assert_eq!(expected.len(), 0);
}

#[test]
fn next_frame() {
    let (mut s, mut r) = channel();
    for i in 0..1000 {
        s.send(i);
        s.send(-i);
        s.next_frame();
    }
    s.close();
    for i in 0..1000 {
        assert_eq!(r.recv(), Ok(&i));
        assert_eq!(r.recv(), Ok(&-i));
        assert_eq!(r.recv(), Err(ReceiverError::EndOfFrame));
        r.next_frame();
    }
    assert_eq!(r.recv(), Err(ReceiverError::ChannelClosed));
}


#[test]
fn next_frame_threaded() {
    let (mut s, mut r) = channel();
    thread::spawn(move || {
        for i in 0..10000 {
            s.send(i);
            s.send(-i);
            s.next_frame();
            thread::sleep_ms(0);
        }
        s.close();
    });
    for i in 0..10000 {
        assert_eq!(r.recv(), Ok(&i));
        assert_eq!(r.recv(), Ok(&-i));
        assert_eq!(r.recv(), Err(ReceiverError::EndOfFrame));
        r.next_frame();
    }
    assert_eq!(r.recv(), Err(ReceiverError::ChannelClosed));
}