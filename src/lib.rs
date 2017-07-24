//! DFQ is a decoupled, fault-tolerant, multi-producer single-consumer queue.
//! DFQ is compatible with `no_std` and is optionally interrupt-safe by being "temporarily lockless".
//!
//! DFQ accepts immutable data items only and does not allow the consumer
//! to remove (pop) or modify the items in the queue.
//! Instead, the consumer can call `mark_completed()` on a queued item
//! to indicate to the producer that the item is ready to be removed from the queue.  
//!
//! Each producer retains ownership of the data items it queues, and only those items. 
//! Thus, a producer is responsible for removing its items from the queue once they have been marked complete by the consumer. 

// #![no_std]

#![allow(dead_code)]
#![feature(alloc, collections)]

extern crate core;

extern crate spin;
extern crate alloc;
extern crate collections;

use spin::{Mutex, MutexGuard};
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::arc::Arc;
use collections::VecDeque;



/// These two types indicate that only immutable items can be queued right now. 
pub type EnqueueParam<T> = Arc<T>;
type QueuedDataType<T> = Arc<QueuedData<T>>;


/// A wrapper around the data that has been queued. This data type is returned when peeking into the queue. 
/// private note:  This must NOT be cloned because it contains an inner Arc reference to the data passed to its constructor!
#[derive(Debug)]
pub struct QueuedData<T> {
    data: Arc<T>,
    completed: AtomicBool, 
}

impl<T> QueuedData<T> {
    /// this does not take ownership of the given `dataref` Arc instance
    /// (nor does it borrow it long term), 
    /// allowing you as the caller to retain ownership of that data. 
    fn new(dataref: Arc<T>) -> QueuedData<T> {
        QueuedData {
            data: dataref.clone(),
            completed: AtomicBool::new(false),
        }
    }

    /// Mark this item as completed. 
    /// This allows the producer (the one that pushed it onto the queue) to know that it's safe to remove it from the queue. 
    pub fn mark_completed(&self) {
        self.completed.store(true, Ordering::SeqCst);
    }

    /// Whether this item has been completed (handled) by the DFQueueConsumer.
    /// If an item is completed, the producer knows it's okay to remove it from the queue.
    pub fn is_completed(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }

    // /// returns true if the given `QueuedData` was locked
    // fn is_locked(&self) -> bool {
    //     match self.data.try_lock() {
    //         Some(_) => false,
    //         None => true,
    //     }
    // }
}

/// taken from Rust's VecDeque implementation
const INITIAL_CAPACITY: usize = 7; // 2^3 - 1


/// The actual queue, an opaque type that cannot be used directly. 
/// The user must use `DFQueueConsumer` and `DFQueueProducer`. 
#[derive(Debug)]
pub struct DFQueue<T> {
    /// the actual inner queue
    queue: Mutex<VecDeque<QueuedDataType<T>>>,
    /// whether this queue has a consumer (it can only have one!!)
    has_consumer: AtomicBool,
}

impl<T> DFQueue<T> {

    /// Creates a new DFQueue. 
    ///
    /// This object cannot be used directly, you must obtain a producer or consumer to the queue
    /// using the functions `into_consumer()` or `obtain_producer()`.
    pub fn new() -> DFQueue<T> {
        DFQueue::with_capacity(INITIAL_CAPACITY)
    }

    /// Creates a new DFQueue with the given initial_capacity. 
    ///
    /// This object cannot be used directly, you must obtain a producer or consumer to the queue
    /// using the functions `into_consumer()` or `obtain_producer()`.
    pub fn with_capacity(initial_capacity: usize) -> DFQueue<T> {
        DFQueue {
            queue: Mutex::new(VecDeque::with_capacity(initial_capacity)),
            has_consumer: AtomicBool::new(false),
        }
    }

    /// Consumes the DFQueue and returns the one and only consumer for this DFQueue. 
    /// It consumes the DFQueue instance because there is only one consumer allowed per DFQueue.
    pub fn into_consumer(self) -> DFQueueConsumer<T> {
        debug_assert!(self.has_consumer.load(Ordering::SeqCst), "DFQueue::into_consumer(): WTF? already had a consumer!");
        self.has_consumer.store(true, Ordering::SeqCst);

        DFQueueConsumer {
            qref: Arc::new(self),
        }
    }

    /// Consumes the DFQueue and returns a producer. 
    /// To obtain another DFQueueProducer for this DFQueue, call `obtain_producer()` on the returned DFQueueProducer. 
    /// DFQueueProducer does not implement the standard Clone trait, to avoid accidentally cloning it implicity. 
    pub fn into_producer(self) -> DFQueueProducer<T> {
        DFQueueProducer {
            qref: Arc::new(self),
            local_backlog: VecDeque::new(), // TODO: may need to wrap this in an Rc<>
        }
    }

}


/// A consumer that can process (peek into) elements in a DFQueue, but not actually remove them.
/// Do not wrap this in an Arc or Mutex, the queue it is already protected by those on the interior. 
///
/// This does not provide a `pop()` method like most queues, 
/// because we do not permit the consumer to remove items from the queue.
/// Instead, we require that an element can only be removed from the queue by the producer who originally enqueued it. 
#[derive(Debug)]
pub struct DFQueueConsumer<T> {
    qref: Arc<DFQueue<T>>,
}

impl<T> DFQueueConsumer<T> {

    /// obtains a DFQueueProducer from this consumer instance, since there can be multiple producers.
    pub fn obtain_producer(&self) -> DFQueueProducer<T> {
        DFQueueProducer {
            qref: self.qref.clone(),
            local_backlog: VecDeque::new(), // TODO: may need to wrap this in an Rc<>
        }
    }

    
    /// internal function that returns the first non-completed item in the queue (queue is locked already)
    fn peek_locked(&self, locked_queue: MutexGuard<VecDeque<QueuedDataType<T>>>) -> Option<QueuedDataType<T>> {
        for e in locked_queue.iter() {
            if e.completed.load(Ordering::SeqCst) {
                continue;
            }
            else {
                return Some(e.clone());
            }
        }

        None
    }



    /// Peeks at the queue in a blocking fashion. 
    /// This will block until it can acquire a lock on the inner queue. 
    /// Returns the first non-completed element in the queue without actually removing it from the queue, 
    /// or `None` if the queue is empty. 
    ///
    /// Note: this is NOT IRQ-safe. 
    /// TODO: use a data guard to automatically release the lock to the returned QueuedData
    pub fn peek_locking(&self) -> Option<QueuedDataType<T>> {
        // first, acquire the lock so we have exclusive access to the inner queue
        let innerq = self.qref.queue.lock(); // blocking call
        
        self.peek_locked(innerq)
    }



    /// Peeks at the queue in a non-blocking fashion. 
    /// Returns the last non-completed element in the queue without actually removing it from the queue. 
    /// Returns `None` if the lock to the inner queue cannot be obtained, or if the queue is empty.
    ///
    /// Note: this is IRQ-safe. 
    /// TODO: use a data guard to automatically release the lock to the returned QueuedData
    pub fn peek_lockless(&self) -> Option<QueuedDataType<T>> {
        // first, try to acquire the queue lock. 
        let lock_result = self.qref.queue.try_lock();
        match lock_result {
            Some(innerq) => {
                self.peek_locked(innerq)
            },
            None => {
                None
            }
        }
    }


}


/// A producer that can enqueue elements into a DFQueue.
/// Do not wrap this in an Arc or Mutex, the queue it is already protected by those on the interior. 
#[derive(Debug)]
pub struct DFQueueProducer<T> {
    qref: Arc<DFQueue<T>>,
    local_backlog: VecDeque<QueuedDataType<T>>,
}


impl<T> DFQueueProducer<T> {

    /// Call this to obtain another DFQueueProducer. 
    /// DFQueueProducer does not implement the standard Clone trait, to avoid accidentally cloning it implicity. 
    pub fn obtain_producer(&self) -> DFQueueProducer<T> {
        DFQueueProducer {
            qref: self.qref.clone(),
            local_backlog: VecDeque::new(), // TODO: may need to wrap this in an Rc<>
        }
    }


    /// Returns a DFQueueConsumer for this queue, if it hasn't yet been obtained 
    /// (either via this function or via `DFQueue::into_consumer()`).
    /// To ensure there is only a single DFQueueConsumer, it will return `None` if there is already a `DFQueueConsumer`.
    pub fn get_consumer(&self) -> Option<DFQueueConsumer<T>> {
        let has_consumer: bool = self.qref.has_consumer.load(Ordering::SeqCst);
        match has_consumer {
            true => None,
            false => {
                self.qref.has_consumer.store(true, Ordering::SeqCst);
                Some(
                    DFQueueConsumer {
                        qref: self.qref.clone(),
                    }
                )
            }
        }
    }


    /// pushes the given `QueuedData`, which is an Arc reference wrapping the actual data item, onto the queue.
    /// This is a blocking call that will acquire the queue lock, write the data, 
    /// and release the lock before returning to the caller. 
    ///
    /// Note: this is NOT IRQ-safe. 
    pub fn enqueue_locking(&mut self, dataref: EnqueueParam<T>) {

        // acquire the lock so we have exclusive access to the inner queue
        let mut innerq = self.qref.queue.lock(); // blocking call

        // 1) move all the previously-backlogged data into the inner queue
        // 2) push the actual `dataref` onto the queue
        innerq.append(&mut self.local_backlog);
        innerq.push_back(Arc::new(QueuedData::new(dataref.clone()))); 

    }


    /// pushes the given `QueuedData`, which is an Arc reference wrapping the actual data item,
    /// onto the back of the queue, only if the queue lock can be acquired immediately.
    /// If the queue lock cannot be acquired, then the given `QueuedData` is put on a local backlog queue 
    /// that is specific to this `DFQueueRef`, which will be merged into the actual inner queue
    /// on the next invocation of any `enqueue` method, provided that the inner queue can be locked immediately at that point. 
    /// Returns true if the given `dataref` was added to the inner queue directly, false if it was added to the backlog.
    ///
    /// Note: this is IRQ-safe. 
    pub fn enqueue_lockless(&mut self, dataref: EnqueueParam<T>) -> bool {



        // first, try to acquire the queue lock. 
        match self.qref.queue.try_lock() {
            Some(mut innerq) => {

                // 1) move all the previously-backlogged data into the inner queue
                // 2) push the actual `dataref` onto the queue
                innerq.append(&mut self.local_backlog);
                innerq.push_back(Arc::new(QueuedData::new(dataref.clone()))); 
                true
            },
            None => {
                // if we can NOT acquire it:
                // add the given `dataref` to this DFQueueRef's producer-local backlog queue
                self.local_backlog.push_back(Arc::new(QueuedData::new(dataref.clone())));
                false
            }
        }

    }

}






// Conditionally compile the module `test` only when the test-suite is run.
#[cfg(test)]
mod test {
    
    extern crate std;

    use std::thread; 
    use super::*;



    #[test]
    // #[should_panic]
    fn simple_test() {

        let queue: DFQueue<usize> = DFQueue::new();

        let mut queue_prod = queue.into_producer();
        let mut queue_cons = queue_prod.get_consumer();
        assert!(queue_cons.is_some(), "First DFQueueConsumer was None!!!");

        // a second call to get_consumer must return None
        assert!(queue_prod.get_consumer().is_none(), "Second DFQueueConsumer wasn't None!!"); 

        let queue_cons = queue_cons.unwrap();

        let mut queue_prod2 = queue_prod.obtain_producer();


        let mut thr_p = thread::spawn( move || {
            let original_data: Vec<Arc<usize>> = vec![Arc::new(1), Arc::new(2), Arc::new(3), Arc::new(4), Arc::new(5)];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod.enqueue_lockless(elem.clone());
                }

                let queued_data: Arc<usize> = Arc::new(255);
                queue_prod.enqueue_lockless(queued_data);
            }
        } );

         let mut thr_p2 = thread::spawn( move || {
            let original_data: Vec<Arc<usize>> = vec![Arc::new(10), Arc::new(11), Arc::new(12), Arc::new(13), Arc::new(14)];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod2.enqueue_lockless(elem.clone());
                }

                let queued_data: Arc<usize> = Arc::new(512);
                queue_prod2.enqueue_lockless(queued_data);
            }
        } );

        let mut thr_c = thread::spawn( move || {
            loop {
                let mut val = queue_cons.peek_locking();
                if let Some(v) = val {
                    println!("peeked: {:?}", v);
                    v.mark_completed();
                }
            }
        } );

        thr_p.join();
        thr_p2.join();
        thr_c.join();
    }
}