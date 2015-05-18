use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::mem;
use std::vec;
use alloc::arc::get_mut;
use atom::*;
use pulse::{Pulse, Signal};

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiverError {
    /// The frame has come to an end, move to the `next`
    /// frame in time.
    EndOfFrame,
    /// The frame has come to an end, and there is no
    /// future frames to move to.
    ChannelClosed
}

struct Block<T> {
    next: AtomSetOnce<Arc<Block<T>>>,
    pulse: Atom<Pulse>,
    signal: AtomSetOnce<Signal>,
    data: Atom<Box<Vec<T>>>
}

impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        while let Some(mut n) = self.next.atom().take() {
            if let Some(n) = get_mut(&mut n) {
                if let Some(next) = n.next.atom().take() {
                    self.next.set_if_none(next);
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }
}

impl<T: Send+Sync> Block<T> {
    fn new(data: Vec<T>) -> Block<T> {
        Block {
            next: AtomSetOnce::empty(),
            pulse: Atom::empty(),
            signal: AtomSetOnce::empty(),
            data: Atom::new(Box::new(data))
        }
    }
}

struct WritePtr<T> {
    current: Arc<Block<T>>,
}

impl<'a, T: Send+Sync> WritePtr<T> {
    /// Create a WritePtr from the Next point embeeded in the Block
    fn from_block(block: Arc<Block<T>>) -> WritePtr<T> {
        WritePtr {
            current: block,
        }
    }

    /// Tries to write a value to the next pointer
    /// on success it returns None meaning the b has been consumed
    /// on failure it returns Some(b) so that b can be written on the next node
    fn write(&self, b: Arc<Block<T>>) -> Option<Arc<Block<T>>> {
        match self.current.next.set_if_none(b) {
            Some(b) => Some(b),
            None => None
        }
    }

    /// Get the next WritePtr, return None if this is the current tail
    fn next(&self) -> Option<WritePtr<T>> {
        self.current.next.dup().map(|next| {
            WritePtr { current: next }
        })
    }

    /// Seek the current tail, this does not guarantee that the value
    /// return is the current tail, just that it was the tail.
    fn tail(&mut self) {
        loop {
            *self = match self.next() {
                Some(v) => v,
                None => return
            };
        }
    }

    /// Seek the current tail, and write block b to it. If that
    /// fails try again until it is written successfully.
    ///
    /// This returns the WritePtr of that block.
    fn append(&mut self, mut b: Arc<Block<T>>) {
        loop {
            self.tail();
            b = match self.write(b) {
                Some(b) => b,
                None => {
                    if let Some(pulse) = self.current.pulse.take() {
                        pulse.pulse();
                    }
                    return;
                }
            };
        }
    }

    /// Seek the tail and wait any waiter
    fn force_wake(&mut self) {
        self.tail();
        if let Some(pulse) = self.current.pulse.take() {
            pulse.pulse();
        }
    }
}

struct ChannelNext<T>(Arc<Channel<T>>, Arc<Block<T>>);

struct Channel<T> {
    // next is used when a channel reaches the end of the frame
    // it is used as a link to the next frame.
    next: AtomSetOnce<Box<ChannelNext<T>>>,
    // Keeps track of the numbe of senders, when it reaches
    // 0 it indicates that we are at the end of the frame.
    senders: AtomicUsize,
}

impl<T: Send+Sync> Channel<T> {
    fn new(senders: usize) -> (Arc<Channel<T>>, Arc<Block<T>>) {
        let next = Arc::new(Channel {
            next: AtomSetOnce::empty(),
            senders: AtomicUsize::new(senders)
        });

        let head = Arc::new(Block::new(Vec::new()));
        (next, head)
    }

    fn next(&self) -> &ChannelNext<T> {
        if self.next.is_none() {
            let (next, head) = Channel::new(0);
            self.next.set_if_none(Box::new(ChannelNext(next, head)));
        }

        self.next.get().map(|x| &*x).unwrap()
    }
}

unsafe impl<T: Sync+Send> Send for Sender<T> {}

pub struct Sender<T: Send+Sync> {
    channel: Arc<Channel<T>>,
    buffer: Vec<T>,
    write: WritePtr<T>
}

fn sender_size<T>() -> usize {
    let size = mem::size_of::<T>();
    if size == 0 {
        2048
    } else if size >= 2048 {
        1
    } else {
        2048 / size
    }
}

impl<T: Send+Sync> Sender<T> {
    #[inline]
    pub fn send(&mut self, value: T) {
        self.buffer.push(value);
        if self.buffer.capacity() == self.buffer.len() {
            self.flush()
        }
    }

    pub fn flush(&mut self) {
        if self.buffer.len() == 0 { return }

        let mut buffer = Vec::with_capacity(sender_size::<T>());
        mem::swap(&mut buffer, &mut self.buffer);
        let block = Arc::new(Block::new(buffer));
        self.write.append(block);
    }

    pub fn next_frame(&mut self) {
        use std::mem;
        self.flush();

        let (mut channel, block) = {
            let &ChannelNext(ref ch, ref block) = self.channel.next();
            (ch.clone(), block.clone())
        };

        channel.senders.fetch_add(1, Ordering::SeqCst);
 
        let mut write = WritePtr::from_block(block);
        mem::swap(&mut write, &mut self.write);
        mem::swap(&mut self.channel, &mut channel);

        let last = channel.senders.fetch_sub(1, Ordering::SeqCst);
        if last == 1 {
            write.force_wake();
        }
    }

    pub fn close(self) {}
}

impl<T: Sync+Send> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        self.channel.senders.fetch_add(1, Ordering::SeqCst);

        Sender {
            channel: self.channel.clone(),
            buffer: Vec::with_capacity(sender_size::<T>()),
            write: WritePtr {
                current: self.write.current.clone(),
            }
        }
    }
}

impl<T: Send+Sync> Drop for Sender<T> {
    fn drop(&mut self) {
        self.flush();

        let last = self.channel.senders.fetch_sub(1, Ordering::SeqCst);
        if last == 1 {
            self.write.force_wake();
        }
    }
}

unsafe impl<T: Sync+Send> Send for Receiver<T> {}

pub struct Receiver<T: Send+Sync> {
    channel: Arc<Channel<T>>,
    current: Arc<Block<T>>,
    block: vec::IntoIter<T>,
    index: usize,
}

impl<T: Send+Sync> Receiver<T> {
    fn next(&mut self) -> bool {
        if let Some(next) = self.current.next.dup() {
            self.block = next.data.take().expect("No data found!").into_iter();
            self.current = next;
            self.index = 0;
            true
        } else {
            false
        }
    }

    pub fn try_recv(&mut self) -> Option<T> {
        loop {
            let next = self.block.next();
            if next.is_some() {
                return next;
            }

            if !self.next() {
                return None
            }
        }
    }

    /// Check to see if the channel has closed
    pub fn closed(&self) -> bool {
        self.channel.senders.load(Ordering::SeqCst) == 0
    }

    #[inline]
    pub fn recv<'a>(&'a mut self) -> Result<T, ReceiverError> {
        loop {
            let next = self.block.next();
            if let Some(next) = next {
                return Ok(next);
            }

            if !self.next() {
                self.signal().wait().unwrap();
            } else {
                continue;
            }

            match (self.channel.senders.load(Ordering::SeqCst) == 0,
                   self.channel.next.is_none(),
                   self.next()) {
                (_,    _,     true)  => continue,
                (true, true,  false) => return Err(ReceiverError::ChannelClosed),
                (true, false, false) => return Err(ReceiverError::EndOfFrame),
                _ => ()
            }
        }
    }

    pub fn signal(&self) -> Signal {
        if let Some(signal) = self.current.signal.dup() {
            return signal;
        } else {
            let (signal, pulse) = Signal::new();
            if self.current.signal.set_if_none(signal).is_none() {
                self.current.pulse.swap(pulse);

                if self.current.next.get().is_some() ||
                   self.channel.senders.load(Ordering::SeqCst) == 0 {
                    self.current.pulse.take().map(|p| p.pulse());
                }
            }
        }

        self.current.signal.dup().unwrap()
    }

    pub fn next_frame(&mut self) -> bool {
        // checkc to see if the channel has been closed.
        if self.channel.senders.load(Ordering::SeqCst) == 0 &&
           self.channel.next.is_none() {
            return false;
        }

        let (channel, block) = {
            let &ChannelNext(ref ch, ref block) = self.channel.next();
            (ch.clone(), block.clone())
        };
 
        self.channel = channel;
        self.current = block;
        self.index = 0;
        true
    }
}

pub fn channel<T: Send+Sync>() -> (Sender<T>, Receiver<T>) {
    let (channel, head) = Channel::new(1);

    let tx = Sender {
        buffer: Vec::with_capacity(sender_size::<T>()),
        channel: channel.clone(),
        write: WritePtr::from_block(head.clone())
    };

    let rx = Receiver {
        channel: channel,
        current: head,
        block: Vec::new().into_iter(),
        index: 0
    };

    (tx, rx)
}
