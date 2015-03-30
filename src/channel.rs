use std::sync::atomic::*;
use std::ptr;
use std::mem;

struct Block<T> {
    next: AtomicPtr<Block<T>>,
    data: Box<[T]>
}

impl<T: Send+Sync> Block<T> {
    fn new(data: Box<[T]>) -> Block<T> {
        Block {
            next: AtomicPtr::new(ptr::null_mut()),
            data: data
        }
    }

    fn next<'a>(&'a self) -> Option<&'a Block<T>> {
        let next = self.next.load(Ordering::Relaxed);
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
        let mut next = self.next.swap(ptr::null_mut(), Ordering::Relaxed);
        // This avoids recursing more then one level
        while !next.is_null() {
            unsafe {
                let next_block: Box<Block<T>> = mem::transmute_copy(&next);
                next = next_block.next.swap(ptr::null_mut(), Ordering::Relaxed);
            }
        }
    }
}

struct WritePtr<'a, T: 'static> {
    next: &'a AtomicPtr<Block<T>>
}

impl<'a, T: Send+Sync> WritePtr<'a, T> {
    /// Create a WritePtr from the Next point embeeded in the Block
    fn from_block(block: &Block<T>) -> WritePtr<T> {
        WritePtr { next: &block.next }
    }

    /// Tries to write a value to the next pointer
    /// on success it returns None meaning the b has been consumed
    /// on failure it returns Some(b) so that b can be written on the next node
    fn write(&self, b: Box<Block<T>>) -> Option<Box<Block<T>>> {
        unsafe {
            let n: *mut Block<T> = mem::transmute_copy(&b);
            let prev = self.next.compare_and_swap(ptr::null_mut(), n, Ordering::SeqCst);
            if prev == ptr::null_mut() {
                // this was stored in the next pointer so forget it
                mem::forget(b);
                None
            } else {
                Some(b)
            }
        }
    }

    /// Get the next WritePtr, return None if this is the current tail
    fn next(&self) -> Option<WritePtr<'a, T>> {
        let next = self.next.load(Ordering::Relaxed);
        if next.is_null() {
            None
        } else {
            unsafe { Some(WritePtr{next: &(*next).next}) }
        }
    }

    /// Seek the current tail, this does not guarantee that the value
    /// return is the current tail, just that it was the tail.
    fn tail(mut self) -> WritePtr<'a, T> {
        loop {
            self = match self.next() {
                Some(v) => v,
                None => return self
            };
        }
    }

    /// Seek the current tail, and write block b to it. If that
    /// fails try again until it is written successfully.
    ///
    /// This returns the WritePtr of that block.
    fn append(mut self, mut b: Box<Block<T>>) -> WritePtr<'a, T> {
        loop {
            self = self.tail();
            b = match self.write(b) {
                Some(b) => b,
                None => return self.tail()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::sync::Arc;
    use channel::{Block, WritePtr};

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
            ptr = ptr.tail();
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
            let ptr = WritePtr::from_block(&mut a);
            ptr.append(b).append(c);
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
            let ptr = WritePtr::from_block(&mut a);
            ptr.append(b).append(c).append(d).append(f);
        }

        assert_eq!(v.load(Ordering::SeqCst), 0);
        drop(a);
        assert_eq!(v.load(Ordering::SeqCst), 5);
    }
}