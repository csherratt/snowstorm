use std::sync::atomic::*;
use std::sync::{Arc, Mutex, Semaphore};
use std::thunk::Thunk;
use std::ptr;
use std::mem;

struct Block<T> {
    next: AtomicPtr<Block<T>>,
    offset: usize,
    data: Box<[T]>
}

impl<T: Send+Sync> Block<T> {
    fn new(data: Box<[T]>) -> Block<T> {
        Block {
            next: AtomicPtr::new(ptr::null_mut()),
            offset: 0,
            data: data
        }
    }

    fn next<'a>(&'a self) -> Option<&'a Block<T>> {
        let next = self.next.load(Ordering::SeqCst);
        if next.is_null() {
            None
        } else {
            let n: &Block<T> = unsafe { mem::transmute_copy(&next) };
            Some(n)
        }
    }
}

#[unsafe_destructor]
impl<T> Drop for Block<T> {
    fn drop(&mut self) {
        let mut next = self.next.swap(ptr::null_mut(), Ordering::SeqCst);
        // This avoids recursing more then one level
        while !next.is_null() {
            unsafe {
                let next_block: Box<Block<T>> = mem::transmute_copy(&next);
                next = next_block.next.swap(ptr::null_mut(), Ordering::SeqCst);
            }
        }
    }
}

struct WritePtr<T> {
    offset: usize,
    next: *const AtomicPtr<Block<T>>
}

impl<T: Send+Sync> WritePtr<T> {
    /// Create a WritePtr from a channel
    fn from_channel(channel: &Channel<T>) -> WritePtr<T> {
        WritePtr::from_block(&channel.head)
    }


    /// Create a WritePtr from the Next point embeeded in the Block
    fn from_block(block: &Block<T>) -> WritePtr<T> {
        WritePtr {
            offset: block.offset,
            next: &block.next
        }
    }

    /// Tries to write a value to the next pointer
    /// on success it returns None meaning the b has been consumed
    /// on failure it returns Some(b) so that b can be written on the next node
    fn write(&self, mut b: Box<Block<T>>) -> Result<usize, Box<Block<T>>> {
        // write our offset into the block, if this succeeds
        // the offset will always be the greatest
        b.offset = self.offset;
        let offset = self.offset + b.data.len();
        unsafe {
            let n: *mut Block<T> = mem::transmute_copy(&b);
            let prev = self.next_ptr().compare_and_swap(ptr::null_mut(), n, Ordering::SeqCst);
            if prev == ptr::null_mut() {
                // this was stored in the next pointer so forget it
                mem::forget(b);
                Ok(offset)
            } else {
                Err(b)
            }
        }
    }

    fn next_ptr(&self) -> &AtomicPtr<Block<T>> {
        unsafe { mem::transmute_copy(&self.next) }
    }

    /// Get the next WritePtr, return None if this is the current tail
    fn next(&self) -> Option<WritePtr<T>> {
        let next = self.next_ptr().load(Ordering::SeqCst);
        if next.is_null() {
            None
        } else {
            unsafe {
                Some( WritePtr {
                    offset: self.offset + (*next).data.len(),
                    next: &(*next).next
                })
            }
        }
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
    fn append(&mut self, mut b: Box<Block<T>>) -> usize {
        loop {
            self.tail();
            b = match self.write(b) {
                Err(b) => b,
                Ok(offset) => return offset
            };
        }
    }
}

struct Waiting {
    index: usize,
    on_wakeup: Thunk<'static, (), ()>
}

struct Channel<T> {
    head: Block<T>,
    // This is eventually consistent value used for the writes
    // If the count is greater then the value a writer just appended
    // the write does not need to enter the wake anyone.
    count: AtomicUsize,
    waiters: Mutex<Vec<Waiting>>
}

impl<T: Send+Sync> Channel<T> {
    fn add_to_waitlist(&self, wait: Waiting) {
        let start = self.count.load(Ordering::SeqCst);
        if start > wait.index {
            (wait.on_wakeup)();
            return;
        }

        {
            let mut guard = self.waiters.lock().unwrap();
            guard.push(wait);
        }

        // since we locked the waiters, it is possible
        // a writer came in and missed a wake up. It's our
        // job to see if that happened
        let end = self.count.load(Ordering::SeqCst);
        if start != end {
            self.wake_waiter(end);
        }
    }

    fn wake_waiter(&self, mut count: usize) {
        loop {
            match self.waiters.try_lock() {
                Ok(mut waiting) => {
                    // wake up only the items that are behind us
                    // swap_remove lets us remove these without
                    // to much overhead.
                    let mut i = 0;
                    while i < waiting.len() {
                        if waiting[i].index <= count {
                            let mut v = waiting.swap_remove(i).on_wakeup;
                            v();
                        } else {
                            i += 1;
                        }
                    }
                },
                // someone else waking people up, they lose and
                // will have to also wake up anyone our payload just
                // woke up.
                Err(_) => return
            }

            // the lock is released now, we have to check if someone
            // hit the Error case above and bailed, if they did
            // we have to do their job for them.
            let current = self.count.load(Ordering::SeqCst);
            if current == count {
                count = current;
                return
            }
        }
    }

    fn advance_count(&self, mut from: usize, to: usize) {
        loop {
            let count = self.count.compare_and_swap(from, to, Ordering::SeqCst);

            // if the count is greater then what we are moving it to
            // it means we lost the race and need to give up
            if count > to {
                return;

            // If the value read was still not the value we expected it means
            // someone has fallen behind. We move out from to their from.
            // There is a case where we try and update the from, jump back to do
            // work meanwhile some does work for us. In that case
            } else if count != from {
                from = count;
                continue

            // we won the race and did the correct update
            } else {
                return self.wake_waiter(to);
            }
        }
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
        // a 0 sized buffer should not be written
        if self.buffer.len() == 0 { return }

        let mut buffer = Vec::with_capacity(sender_size::<T>());
        mem::swap(&mut buffer, &mut self.buffer);
        let block = Box::new(Block::new(buffer.into_boxed_slice()));
        let mut to = self.write.append(block);
        let mut from = self.write.offset;

        self.channel.advance_count(from, to);
    }
}

impl<T: Sync+Send> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender {
            channel: self.channel.clone(),
            buffer: Vec::with_capacity(sender_size::<T>()),
            write: WritePtr {
                offset: self.write.offset,
                next: self.write.next
            }
        }
    }
}

#[unsafe_destructor]
impl<T: Send+Sync> Drop for Sender<T> {
    fn drop(&mut self) { self.flush() }
}

unsafe impl<T: Sync+Send> Send for Receiver<T> {}

pub struct Receiver<T> {
    channel: Arc<Channel<T>>,
    current: *const Block<T>,
    offset: usize,
    sema: Arc<Semaphore>
}

impl<T: Send+Sync> Receiver<T> {
    fn next(&mut self) -> bool {
        unsafe {
            let next = (*self.current).next.load(Ordering::SeqCst);
            if !next.is_null() {
                self.current = next;
            }
            !next.is_null()
        }
    }

    fn idx(&self) -> usize { unsafe { self.offset - (*self.current).offset } }
    fn idx_post_inc(&mut self) -> usize {
        let idx = self.idx();
        self.offset += 1;
        return idx;
    }

    pub fn try_recv<'a>(&'a mut self) -> Option<&'a T> { unsafe {
        if self.pending() {
            let idx = self.idx_post_inc();
            Some(&(*self.current).data[idx])
        } else {
            None
        }
    }}

    /// check to see if there is data pending
    pub fn pending(&mut self) -> bool {
        unsafe {
            if (*self.current).data.len() <= self.idx() {
                self.next()
            } else {
                true
            }
        }
    }

    pub fn recv<'a>(&'a mut self) -> Option<&'a T> {
        if !self.pending() {
            let sema = self.sema.clone();
            self.wait(move || { sema.release() });
            self.sema.acquire();
        }
        self.try_recv()
    }

    fn wait<F>(&self, f: F) where F: FnOnce(), F: Send + 'static {
        let waiting = Waiting {
            index: self.offset,
            on_wakeup: Box::new(f)
        };
        self.channel.add_to_waitlist(waiting);
    }
}

impl<T: Sync+Send> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        Receiver {
            channel: self.channel.clone(),
            current: self.current,
            offset: self.offset,
            sema: Arc::new(Semaphore::new(0))
        }
    }
}

pub fn channel<T: Send+Sync>() -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel {
        head: Block::new(Vec::new().into_boxed_slice()),
        count: AtomicUsize::new(0),
        waiters: Mutex::new(Vec::new())
    });
    let channel_ptr = &channel.head as *const Block<T>;
    (Sender {
        buffer: Vec::new(),
        channel: channel.clone(),
        write: WritePtr::from_channel(&channel)
    },
    Receiver {
        channel: channel,
        current: channel_ptr,
        offset: 0,
        sema: Arc::new(Semaphore::new(0))
    })
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::collections::BTreeSet;
    use std::sync::atomic::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use test::{Bencher, black_box};
    use channel::{Block, WritePtr, channel};

    #[test]
    fn write_ptr_write() {
        let mut a = Box::new(Block::new(Box::new([1i32])));
        let b = Box::new(Block::new(Box::new([2i32])));
        let c = Box::new(Block::new(Box::new([3i32])));

        {
            let ptr = WritePtr::from_block(&mut a);
            assert_eq!(ptr.write(b).ok(), Some(1));
            assert!(ptr.write(c).is_err());
        }

        assert!(*a.data == vec!(1)[..]);
        assert!(*a.next().unwrap().data == vec!(2)[..]);
        assert!(a.next().unwrap().next().is_none());

    }

    #[test]
    fn write_ptr_write_tail() {
        let mut a = Box::new(Block::new(Box::new([1i32])));
        let b = Box::new(Block::new(Box::new([2i32])));
        let c = Box::new(Block::new(Box::new([3i32])));

        {
            let mut ptr = WritePtr::from_block(&mut a);
            assert_eq!(ptr.write(b).ok(), Some(1));
            ptr.tail();
            assert_eq!(ptr.write(c).ok(), Some(2));
        }

        assert!(*a.data == vec!(1)[..]);
        assert!(*a.next().unwrap().data == vec!(2)[..]);
        assert!(*a.next().unwrap().next().unwrap().data == vec!(3)[..]);
    }

    #[test]
    fn write_ptr_append() {
        let mut a = Box::new(Block::new(Box::new([1i32])));
        let b = Box::new(Block::new(Box::new([2i32])));
        let c = Box::new(Block::new(Box::new([3i32])));

        {
            let mut ptr = WritePtr::from_block(&mut a);
            ptr.append(b);
            ptr.append(c);
        }

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
        let mut a = Box::new(Block::new(Box::new([Canary(v.clone())])));
        let b = Box::new(Block::new(Box::new([Canary(v.clone())])));
        let c = Box::new(Block::new(Box::new([Canary(v.clone())])));
        let d = Box::new(Block::new(Box::new([Canary(v.clone())])));
        let f = Box::new(Block::new(Box::new([Canary(v.clone())])));

        {
            let mut ptr = WritePtr::from_block(&mut a);
            ptr.append(b);
            ptr.append(c);
            ptr.append(d);
            ptr.append(f);
        }

        assert_eq!(v.load(Ordering::SeqCst), 0);
        drop(a);
        assert_eq!(v.load(Ordering::SeqCst), 5);
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
        let (mut s, _) = channel();

        for i in (0..1000) {
            s.send(Canary(v.clone()));
            if i & 0xF == 8 {
                s.flush();
            }
        }

        assert_eq!(v.load(Ordering::SeqCst), 0);
        drop(s);
        assert_eq!(v.load(Ordering::SeqCst), 1000);
    }

    #[test]
    fn hundred_writers() {
        let (mut s0, mut r) = channel();

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
    fn waiting_recv() {
        let (mut s, mut r) = channel();
        let barrier0 = Arc::new(Barrier::new(2));
        let barrier1 = Arc::new(Barrier::new(2));

        let barrier0_sender = barrier0.clone();
        let barrier1_sender = barrier1.clone();
        thread::spawn(move || {
            barrier0_sender.wait();
            s.send(0i32);
            s.flush();
            barrier1_sender.wait();
        });

        let v = Arc::new(AtomicUsize::new(0));
        assert!(r.try_recv().is_none());
        let v1 = v.clone();
        assert_eq!(0, v.load(Ordering::SeqCst));
        r.wait(move || { v1.fetch_add(1, Ordering::SeqCst); });
        assert_eq!(0, v.load(Ordering::SeqCst));
        barrier0.wait();
        barrier1.wait();
        assert_eq!(1, v.load(Ordering::SeqCst));
        assert!(r.try_recv().is_some());
    }

    #[bench]
    fn bench_channel_send(bench: &mut Bencher) {
        let (mut s, _) = channel();

        let mut i: i32 = 0;

        bench.iter(|| {
            s.send(i);
            i += 1;
        });

        bench.bytes = 4;
    }

    #[bench]
    fn bench_channel_recv(bench: &mut Bencher) {
        let (mut s, mut r) = channel();

        // this might need to be bigger
        for i in (0..10_000_000i32) {
            s.send(i);
        }
        s.flush();

        bench.iter(|| {
            black_box(r.try_recv());
        });

        // iff this panics it means we did not set up enough elements...
        r.try_recv().unwrap();
        bench.bytes = 4;
    }
}