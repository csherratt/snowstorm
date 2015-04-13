
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::mem;
use atom::*;

struct Inner {
    head: Atom<Listener, Box<Listener>>,
    wake: Mutex<Option<Wake>>,
}

struct Listener {
    next: Option<Box<Listener>>,
    select: Arc<Inner>
}

impl GetNextMut for Box<Listener> {
    type NextPtr = Option<Box<Listener>>;
    fn get_next(&mut self) -> &mut Option<Box<Listener>> {
        &mut self.next
    }
}

//
pub struct Select {
    select: Arc<Inner>,
    woken: Option<Box<Listener>>
}

pub struct Handle(Box<Listener>);

impl Handle {
    pub fn trigger(self) {
        let select = self.0.select.clone();
        if select.head.replace_and_set_next(self.0, Ordering::SeqCst) {
            let mut guard = select.wake.lock().unwrap();
            guard.take().map(|x| {
                x.trigger()
            });
        }
    }

    pub fn id(&self) -> usize {
        unsafe { mem::transmute_copy(&self.0) }
    }
}

fn fetch_head(woken: &mut Option<Box<Listener>>, select: &Inner) -> bool {
    if woken.is_some() {
        return true;
    }
    *woken = select.head.take(Ordering::SeqCst);
    woken.is_some()
}

impl Select {
    pub fn new() -> Select {
        let inner = Arc::new(Inner{
            head: Atom::empty(),
            wake: Mutex::new(None)
        });
        Select {
            select: inner,
            woken: None
        }
    }

    pub fn handle(&self) -> Handle {
        Handle(Box::new(Listener {
            next: None,
            select: self.select.clone()
        }))
    }

    pub fn on_ready(&mut self, wake: Wake) {
        let mut guard = self.select.wake.lock().unwrap();
        if fetch_head(&mut self.woken, &self.select) {
            wake.trigger();
        } else {
            *guard = Some(wake);
        }
    }

    pub fn ready(&mut self) -> Option<Handle> {
        fetch_head(&mut self.woken, &self.select);
        self.woken.take().map(|mut handle| {
            self.woken = handle.next.take();
            Handle(handle)
        })
    }
}

impl Iterator for Select {
    type Item = Handle;
    fn next(&mut self) -> Option<Handle> {
        if let Some(h) = self.ready() {
            return Some(h);
        }
        self.on_ready(Wake::thread());
        thread::park();
        self.ready()
    }
}


pub enum Wake {
    Select(Handle),
    Thread(Thread),
}

impl Wake {
    pub fn thread() -> Wake {
        Wake::Thread(thread::current())
    }

    pub fn trigger(self) {
        match self {
            Wake::Select(handle) => handle.trigger(),
            Wake::Thread(thread) => thread.unpark()
        }
    }
}