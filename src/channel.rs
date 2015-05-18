use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::mem;
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
    data: Box<[T]>
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
    fn new(data: Box<[T]>) -> Block<T> {
        Block {
            next: AtomSetOnce::empty(),
            pulse: Atom::empty(),
            signal: AtomSetOnce::empty(),
            data: data
        }
    }

    #[cfg(test)]
    fn next<'a>(&'a self) -> Option<&'a Block<T>> {
        self.next.get()
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

        let head = Arc::new(Block::new(Vec::new().into_boxed_slice()));
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
        let block = Arc::new(Block::new(buffer.into_boxed_slice()));
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
    offset: usize,
    index: usize,
}

impl<T: Send+Sync> Receiver<T> {
    fn next(&mut self) -> bool {
        if let Some(next) = self.current.next.dup() {
            self.current = next;
            self.offset += self.index;
            self.index = 0;
            true
        } else {
            false
        }
    }

    fn idx(&self) -> usize { self.index }
    fn idx_post_inc(&mut self) -> usize {
        let idx = self.idx();
        self.index += 1;
        return idx;
    }

    pub fn try_recv(&mut self) -> Option<&T> {
        if self.pending() {
            let idx = self.idx_post_inc();
            Some(&self.current.data[idx])
        } else {
            None
        }
    }

    /// check to see if there is data pending
    pub fn pending(&mut self) -> bool {
        if self.current.data.len() <= self.idx() {
            self.next()
        } else {
            true
        }
    }

    /// Check to see if the channel has closed
    pub fn closed(&self) -> bool {
        self.channel.senders.load(Ordering::SeqCst) == 0
    }

    #[inline]
    pub fn recv<'a>(&'a mut self) -> Result<&'a T, ReceiverError> {
        loop {
            if !self.pending() {
                self.signal().wait().unwrap();
            } else {
                break;
            }

            match (self.channel.senders.load(Ordering::SeqCst) == 0,
                   self.channel.next.is_none(),
                   self.pending()) {
                (_,    _,     true)  => break,
                (true, true,  false) => return Err(ReceiverError::ChannelClosed),
                (true, false, false) => return Err(ReceiverError::EndOfFrame),
                _ => ()
            }
        }

        let idx = self.idx_post_inc();
        Ok(&self.current.data[idx])
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
        self.offset = 0;
        self.index = 0;
        true
    }
}

impl<T: Send+Sync+Copy+'static> Receiver<T> {
    pub fn copy_iter<'a>(&'a mut self) -> CopyIter<'a, T> {
        CopyIter {
            inner: self
        }
    }
}

impl<T: Sync+Send> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        Receiver {
            channel: self.channel.clone(),
            current: self.current.clone(),
            offset: self.offset,
            index: self.index
        }
    }
}

pub struct CopyIter<'a, T:Send+Sync+'static> {
    inner: &'a mut Receiver<T>
}

impl<'a, T: Send+Sync+Copy+'static> Iterator for CopyIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.inner.try_recv().map(|x| *x)
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
        offset: 0,
        index: 0
    };

    (tx, rx)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::sync::Arc;
    use channel::{Block, WritePtr};

    #[test]
    fn write_ptr_write() {
        let a = Arc::new(Block::new(Box::new([1i32])));
        let b = Arc::new(Block::new(Box::new([2i32])));
        let c = Arc::new(Block::new(Box::new([3i32])));

        let ptr = WritePtr::from_block(a.clone());
        assert!(ptr.write(b).is_none());
        assert!(ptr.write(c).is_some());

        assert!(*a.data == vec!(1)[..]);
        assert!(*a.next().unwrap().data == vec!(2)[..]);
        assert!(a.next().unwrap().next().is_none());

    }

    #[test]
    fn write_ptr_write_tail() {
        let a = Arc::new(Block::new(Box::new([1i32])));
        let b = Arc::new(Block::new(Box::new([2i32])));
        let c = Arc::new(Block::new(Box::new([3i32])));

        let mut ptr = WritePtr::from_block(a.clone());
        assert!(ptr.write(b).is_none());
        ptr.tail();
        assert!(ptr.write(c).is_none());

        assert!(*a.data == vec!(1)[..]);
        assert!(*a.next().unwrap().data == vec!(2)[..]);
        assert!(*a.next().unwrap().next().unwrap().data == vec!(3)[..]);
    }

    #[test]
    fn write_ptr_append() {
        let a = Arc::new(Block::new(Box::new([1i32])));
        let b = Arc::new(Block::new(Box::new([2i32])));
        let c = Arc::new(Block::new(Box::new([3i32])));

        let mut ptr = WritePtr::from_block(a.clone());
        ptr.append(b);
        ptr.append(c);

        assert!(*a.data == vec!(1)[..]);
        assert!(*a.next().unwrap().data == vec!(2)[..]);
        assert!(*a.next().unwrap().next().unwrap().data == vec!(3)[..]);
    }

    struct Canary(Arc<AtomicUsize>);

    impl Drop for Canary {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn block_drop() {
        let v = Arc::new(AtomicUsize::new(0));
        let a = Arc::new(Block::new(Box::new([Canary(v.clone())])));
        let b = Arc::new(Block::new(Box::new([Canary(v.clone())])));
        let c = Arc::new(Block::new(Box::new([Canary(v.clone())])));
        let d = Arc::new(Block::new(Box::new([Canary(v.clone())])));
        let e = Arc::new(Block::new(Box::new([Canary(v.clone())])));

        let mut ptr = WritePtr::from_block(a.clone());
        ptr.append(b);
        ptr.append(c);
        ptr.append(d);
        ptr.append(e);

        assert_eq!(v.load(Ordering::SeqCst), 0);
        drop(ptr);
        drop(a);
        assert_eq!(v.load(Ordering::SeqCst), 5);
    }
}