use std::sync::atomic::*;
use std::sync::{Arc, Mutex};
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
        WritePtr {
            offset: 0,
            next: &channel.head
        }
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
    fn write(&self, mut b: Box<Block<T>>) -> Option<Box<Block<T>>> {
        // write our offset into the block, if this succeeds
        // the offset will always be the greatest
        b.offset = self.offset;
        unsafe {
            let n: *mut Block<T> = mem::transmute_copy(&b);
            let prev = self.next_ptr().compare_and_swap(ptr::null_mut(), n, Ordering::SeqCst);
            if prev == ptr::null_mut() {
                // this was stored in the next pointer so forget it
                mem::forget(b);
                None
            } else {
                Some(b)
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
    fn append(&mut self, mut b: Box<Block<T>>) {
        loop {
            self.tail();
            b = match self.write(b) {
                Some(b) => b,
                None => return
            };
        }
    }
}

struct Waiting {
    index: usize,
    on_wakeup: Box<FnOnce(usize)+Send>
}

struct Channel<T> {
    head: AtomicPtr<Block<T>>,
    // This is eventually consistent value used for the writes
    // If the count is greater then the value a writer just appended
    // the write does not need to enter the wake anyone.
    count: AtomicUsize,
    waiters: Mutex<Vec<Waiting>>
}

#[unsafe_destructor]
impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        unsafe {
            let head = self.head.swap(ptr::null_mut(), Ordering::SeqCst);
            if !head.is_null() {
                let head: Box<Block<T>> = mem::transmute_copy(&head);
                drop(head);
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
        if self.buffer.len() != 0 {
            let mut buffer = Vec::with_capacity(sender_size::<T>());
            mem::swap(&mut buffer, &mut self.buffer);
            let block = Box::new(Block::new(buffer.into_boxed_slice()));
            self.write.append(block);
        }
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

pub struct Receiver<T> {
    channel: Arc<Channel<T>>,
    current: *const Block<T>,
    offset: usize
}

impl<T: Send+Sync> Receiver<T> {
    pub fn recv(&mut self) -> Option<&T> {
        if self.offset == 0 && self.current.is_null() {
            unsafe { self.current = mem::transmute_copy(&self.channel.head); };
        }

        loop {
            if self.current.is_null() {
                return None;
            } else {
                unsafe {
                    let idx = self.offset - (*self.current).offset;
                    if (*self.current).data.len() <= idx {
                        let next = (*self.current).next.load(Ordering::SeqCst);
                        self.current = next;
                    } else {
                        self.offset += 1;
                        return Some(&(*self.current).data[idx]);
                    }
                }
            }
        }
    }

    pub fn restart(&mut self) {
        self.current = ptr::null();
        self.offset = 0;
    }
}

impl<T: Sync+Send> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        Receiver {
            channel: self.channel.clone(),
            current: self.current,
            offset: self.offset
        }
    }
}

fn channel<T: Send+Sync>() -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel {
        head: AtomicPtr::new(ptr::null_mut()),
        count: AtomicUsize::new(0),
        waiters: Mutex::new(Vec::new())
    });
    (Sender {
        buffer: Vec::new(),
        channel: channel.clone(),
        write: WritePtr::from_channel(&channel)
    },
    Receiver {
        channel: channel,
        current: ptr::null(),
        offset: 0
    })
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::collections::BTreeSet;
    use std::sync::atomic::*;
    use std::sync::{Arc, Barrier};
    use std::thread::Thread;
    use test::{Bencher, black_box};
    use channel::{Block, WritePtr, channel};

    #[test]
    fn write_ptr_write() {
        let mut a = Box::new(Block::new(Box::new([1i32])));
        let b = Box::new(Block::new(Box::new([2i32])));
        let c = Box::new(Block::new(Box::new([3i32])));

        {
            let ptr = WritePtr::from_block(&mut a);
            assert!(ptr.write(b).is_none());
            ptr.write(c).unwrap();
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
            assert!(ptr.write(b).is_none());
            ptr.tail();
            assert!(ptr.write(c).is_none());
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
            assert_eq!(r.recv(), Some(&i));
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
            assert_eq!(r0.recv(), Some(&i));
        }
        for i in (0..1000) {
            assert_eq!(r1.recv(), Some(&i));
        }
    }

    #[test]
    fn channel_recv_restart() {
        let (mut s, mut r) = channel();
        for i in (0..1000) {
            s.send(i);
        }

        s.flush();

        for i in (0..1000) {
            assert_eq!(r.recv(), Some(&i));
        }

        r.restart();
        for i in (0..1000) {
            assert_eq!(r.recv(), Some(&i));
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
            assert_eq!(r.recv(), Some(&i));
        }
    }

    #[test]
    fn channel_drop() {
        let v = Arc::new(AtomicUsize::new(0));
        let (mut s, _) = channel();

        for i in (0..1_000) {
            s.send(Canary(v.clone()));
        }

        assert_eq!(v.load(Ordering::SeqCst), 0);
        drop(s);
        assert_eq!(v.load(Ordering::SeqCst), 1_000);
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
            Thread::spawn(move || {
                start.wait();
                for j in (0..100) {
                    s.send(i*100 + j);
                    s.flush();
                }
                end.wait();
            });
        }

        let mut expected: BTreeSet<i32> = (0..10_000).collect();
        while let Some(i) = r.recv() {
            assert_eq!(expected.remove(i), true);
        }

        assert_eq!(expected.len(), 0);
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
            black_box(r.recv());
        });

        // iff this panics it means we did not set up enough elements...
        r.recv().unwrap();
        bench.bytes = 4;
    }
}