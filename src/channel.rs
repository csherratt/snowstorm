use std::sync::{Arc, Mutex, Semaphore};
use std::sync::atomic::AtomicUsize;
use std::mem;
use std::thread;
use alloc::boxed::FnBox;
use alloc::arc::get_mut;
use atom::*;
use select::*;

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
    next: AtomSetOnce<Block<T>, Arc<Block<T>>>,
    data: Box<[T]>
}

impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        while let Some(mut n) = self.next.atom().take(Ordering::SeqCst) {
            if let Some(n) = get_mut(&mut n) {
                if let Some(next) = n.next.atom().take(Ordering::SeqCst) {
                    self.next.set_if_none(next, Ordering::SeqCst);
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
            data: data
        }
    }

    #[cfg(test)]
    fn next<'a>(&'a self) -> Option<&'a Block<T>> {
        self.next.get(Ordering::SeqCst)
    }
}

struct WritePtr<T> {
    current: Arc<Block<T>>,
    offset: usize
}

impl<'a, T: Send+Sync> WritePtr<T> {
    /// Create a WritePtr from the Next point embeeded in the Block
    fn from_block(block: Arc<Block<T>>) -> WritePtr<T> {
        WritePtr {
            current: block,
            offset: 0
        }
    }

    /// Tries to write a value to the next pointer
    /// on success it returns None meaning the b has been consumed
    /// on failure it returns Some(b) so that b can be written on the next node
    fn write(&self, b: Arc<Block<T>>) -> Option<Arc<Block<T>>> {
        match self.current.next.set_if_none(b, Ordering::SeqCst) {
            Some(b) => Some(b),
            None => None
        }
    }

    /// Get the next WritePtr, return None if this is the current tail
    fn next(&self) -> Option<WritePtr<T>> {
        self.current.next.dup(Ordering::SeqCst).map(|next| {
            let len = next.data.len();
            WritePtr {
                current: next,
                offset: self.offset + len
            }
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
    fn append(&mut self, mut b: Arc<Block<T>>) -> usize {
        loop {
            let len = b.data.len();
            self.tail();
            b = match self.write(b) {
                Some(b) => b,
                None => return self.offset + len
            };
        }
    }
}

struct Waiting {
    index: usize,
    wake: Wake
}

struct ChannelNext<T>(Arc<Channel<T>>, Arc<Block<T>>);

struct Channel<T> {
    // next is used when a channel reaches the end of the frame
    // it is used as a link to the next frame.
    next: AtomSetOnce<ChannelNext<T>, Box<ChannelNext<T>>>,
    // Keeps track of the numbe of senders, when it reaches
    // 0 it indicates that we are at the end of the frame.
    senders: AtomicUsize,

    // This is eventually consistent value used for the writes
    // If the count is greater then the value a writer just appended
    // the write does not need to enter the wake anyone.
    count: AtomicUsize,
    waiters: Mutex<Vec<Waiting>>
}

impl<T: Send+Sync> Channel<T> {
    fn new(senders: usize) -> (Arc<Channel<T>>, Arc<Block<T>>) {
        let next = Arc::new(Channel {
            next: AtomSetOnce::empty(),
            count: AtomicUsize::new(0),
            senders: AtomicUsize::new(senders),
            waiters: Mutex::new(Vec::new())
        });

        let head = Arc::new(Block::new(Vec::new().into_boxed_slice()));
        (next, head)
    }

    fn add_to_waitlist(&self, index: usize, wake: Wake) {
        let start = self.count.load(Ordering::SeqCst);
        if start > index {
            wake.trigger();
            return;
        }

        {
            let mut guard = self.waiters.lock().unwrap();
            guard.push(Waiting{
                index: index,
                wake: wake
            });
        }

        // since we locked the waiters, it is possible
        // a writer came in and missed a wake up. It's our
        // job to see if that happened
        let end = self.count.load(Ordering::SeqCst);
        let force = self.senders.load(Ordering::SeqCst) == 0;
        if start != end || force {
            self.wake_waiter(end, force);
        }
    }

    fn wake_waiter(&self, mut count: usize, force: bool) -> usize {
        let mut woke = 0;
        loop {
            match self.waiters.try_lock() {
                Ok(mut waiting) => {
                    // wake up only the items that are behind us
                    // swap_remove lets us remove these without
                    // to much overhead.
                    let mut i = 0;
                    while i < waiting.len() {
                        if waiting[i].index < count || force {
                            woke += 1;
                            waiting.swap_remove(i).wake.trigger();
                        } else {
                            i += 1;
                        }
                    }
                },
                // someone else waking people up, they lose and
                // will have to also wake up anyone our payload just
                // woke up.
                Err(_) => return woke
            }

            // the lock is released now, we have to check if someone
            // hit the Error case above and bailed, if they did
            // we have to do their job for them.
            let current = self.count.load(Ordering::SeqCst);
            if current == count {
                return woke;
            } else {
                count = current;
            }
        }
    }

    fn force_wake(&self) -> usize {
        let count = self.count.load(Ordering::SeqCst);
        self.wake_waiter(count, true)
    }

    fn advance_count(&self, mut from: usize, to: usize) -> usize {
        loop {
            let count = self.count.compare_and_swap(from, to, Ordering::SeqCst);

            // if the count is greater then what we are moving it to
            // it means we lost the race and need to give up
            if count > to {
                return 0;

            // If the value read was still not the value we expected it means
            // someone has fallen behind. We move out from to their from.
            // There is a case where we try and update the from, jump back to do
            // work meanwhile some does work for us. In that case
            } else if count != from {
                from = count;
                continue

            // we won the race and did the correct update
            } else {
                return self.wake_waiter(to, false);
            }
        }
    }

    fn next(&self) -> &ChannelNext<T> {
        if self.next.get(Ordering::SeqCst).is_none() {
            let (next, head) = Channel::new(0);
            self.next.set_if_none(
                Box::new(ChannelNext(next, head)),
                Ordering::SeqCst
            );
        }

        self.next.get(Ordering::SeqCst).map(|x| &*x).unwrap()
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
        8192
    } else if size >= 8192 {
        1
    } else {
        8192 / size
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
        let to = self.write.append(block);
        let from = self.write.offset;

        self.channel.advance_count(from, to);
    }

    pub fn next_frame(&mut self) {
        self.flush();

        let (channel, block) = {
            let &ChannelNext(ref ch, ref block) = self.channel.next();
            (ch.clone(), block.clone())
        };

        channel.senders.fetch_add(1, Ordering::SeqCst);
 
        let old = self.channel.clone();
        self.channel = channel;
        self.write = WritePtr::from_block(block);

        let last = old.senders.fetch_sub(1, Ordering::SeqCst);
        if last == 1 {
            old.force_wake();
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
                offset: self.write.offset
            }
        }
    }
}

#[unsafe_destructor]
impl<T: Send+Sync> Drop for Sender<T> {
    fn drop(&mut self) {
        self.flush();

        let last = self.channel.senders.fetch_sub(1, Ordering::SeqCst);
        if last == 1 {
            self.channel.force_wake();
        }
    }
}

unsafe impl<T: Sync+Send> Send for Receiver<T> {}

pub struct Receiver<T: Send+Sync> {
    channel: Arc<Channel<T>>,
    current: Arc<Block<T>>,
    offset: usize,
    index: usize,
    sema: Arc<Semaphore>
}

impl<T: Send+Sync> Receiver<T> {
    fn next(&mut self) -> bool {
        if let Some(next) = self.current.next.dup(Ordering::SeqCst) {
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

    pub fn recv<'a>(&'a mut self) -> Result<&'a T, ReceiverError> {
        loop {
            if !self.pending() {
                self.wait(Wake::thread());
                thread::park();
            } else {
                break;
            }

            match (self.channel.senders.load(Ordering::SeqCst) == 0,
                   self.channel.next.get(Ordering::SeqCst).is_none(),
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

    pub fn wait(&self, wake: Wake) {
        self.channel.add_to_waitlist(
            self.offset + self.index,
            wake
        );
    }

    pub fn next_frame(&mut self) -> bool {
        // checkc to see if the channel has been closed.
        if self.channel.senders.load(Ordering::SeqCst) == 0 &&
           self.channel.next.get(Ordering::SeqCst).is_none() {
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

impl<T: Sync+Send> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        Receiver {
            channel: self.channel.clone(),
            current: self.current.clone(),
            offset: self.offset,
            index: self.index,
            sema: Arc::new(Semaphore::new(0))
        }
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
        index: 0,
        sema: Arc::new(Semaphore::new(0))
    };

    (tx, rx)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use channel::{Block, WritePtr, channel, ReceiverError};

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