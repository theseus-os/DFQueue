//! Ported from Rust's std::sync::mpsc::Queue.


use core::sync::atomic::{AtomicPtr, Ordering};
use core::cell::UnsafeCell;
use core::ptr;
use alloc::boxed::Box;


/// A result of the `pop` function.
pub enum PopResult<T> {
    /// Some data has been popped
    Data(T),
    /// The queue is empty
    Empty,
    /// The queue is in an inconsistent state. Popping data should succeed, but
    /// some pushers have yet to make enough progress in order allow a pop to
    /// succeed. It is recommended that a pop() occur "in the near future" in
    /// order to see if the sender has made progress or not
    Inconsistent,
}

struct Node<T> {
    next: AtomicPtr<Node<T>>,
    value: Option<T>,
}

/// The multi-producer single-consumer structure. This is not cloneable, but it
/// may be safely shared so long as it is guaranteed that there is only one
/// popper at a time (many pushers are allowed).
pub struct MpscQueue<T> {
    head: AtomicPtr<Node<T>>,
    tail: UnsafeCell<*mut Node<T>>,
}

unsafe impl<T: Send> Send for MpscQueue<T> { }
unsafe impl<T: Send> Sync for MpscQueue<T> { }

impl<T> Node<T> {
    unsafe fn new(v: Option<T>) -> *mut Node<T> {
        Box::into_raw(box Node {
            next: AtomicPtr::new(ptr::null_mut()),
            value: v,
        })
    }
}

impl<T> MpscQueue<T> {
    /// Creates a new queue that is safe to share among multiple producers and
    /// one consumer.
    pub fn new() -> MpscQueue<T> {
        let stub = unsafe { Node::new(None) };
        MpscQueue {
            head: AtomicPtr::new(stub),
            tail: UnsafeCell::new(stub),
        }
    }

    /// Pushes a new value onto this queue.
    pub fn push(&self, t: T) {
        unsafe {
            let n = Node::new(Some(t));
            let prev = self.head.swap(n, Ordering::AcqRel);
            (*prev).next.store(n, Ordering::Release);
        }
    }

    /// Pops some data from this queue.
    ///
    /// Note that the current implementation means that this function cannot
    /// return `Option<T>`. It is possible for this queue to be in an
    /// inconsistent state where many pushes have succeeded and completely
    /// finished, but pops cannot return `Some(t)`. This inconsistent state
    /// happens when a pusher is pre-empted at an inopportune moment.
    ///
    /// This inconsistent state means that this queue does indeed have data, but
    /// it does not currently have access to it at this time.
    fn pop_inner(&self) -> PopResult<T> {
        unsafe {
            let tail = *self.tail.get();
            let next = (*tail).next.load(Ordering::Acquire);

            if !next.is_null() {
                *self.tail.get() = next;
                assert!((*tail).value.is_none());
                assert!((*next).value.is_some());
                let ret = (*next).value.take().unwrap();
                let _: Box<Node<T>> = Box::from_raw(tail);
                return PopResult::Data(ret);
            }

            if self.head.load(Ordering::Acquire) == tail { PopResult::Empty } else { PopResult::Inconsistent }
        }
    }

    /// Pops data from this queue.
    pub fn pop(&self) -> Option<T> {
        match self.pop_inner() {
            PopResult::Data(d) => Some(d),
            _ => None,
        }
    }
}

impl<T> Drop for MpscQueue<T> {
    fn drop(&mut self) {
        unsafe {
            let mut cur = *self.tail.get();
            while !cur.is_null() {
                let next = (*cur).next.load(Ordering::Relaxed);
                let _: Box<Node<T>> = Box::from_raw(cur);
                cur = next;
            }
        }
    }
}
