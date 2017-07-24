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

#![feature(alloc, collections)]
#![allow(dead_code)]

// #![no_std] // use core instead of std

extern crate spin;
extern crate alloc;
extern crate collections;

use spin::{Mutex, MutexGuard};
use alloc::arc::Arc;
use collections::VecDeque;

// use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicBool, Ordering};


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

    // /// returns true if the given `QueuedData` was locked
    // fn is_locked(&self) -> bool {
    //     match self.data.try_lock() {
    //         Some(_) => false,
    //         None => true,
    //     }
    // }
}


/// The actual queue structure. The DFQueueRef is a reference to this actual queue. 
/// The user should use the DFQueueRef type, not this one. 
struct DFQueue<T> {
    queue: Mutex<VecDeque<QueuedDataType<T>>>,
}


/// There is one DFQueueRef per producer and one per consumer.
/// Do not wrap this in an Arc or Mutex, the queue it is already protected by those on the interior. 
///
/// Note: this does not provide a `pop()` method like most queues, 
/// because we do not permit the consumer to remove items from the queue.
/// Instead, we require that an element can only be removed from the queue by the producer who originally enqueued it. 
pub struct DFQueueRef<T> {
    qref: Arc<DFQueue<T>>,
    local_backlog: VecDeque<QueuedDataType<T>>,
}




impl<T> DFQueueRef<T> {

    /// creates a new DFQueue with the given initial_capacity 
    /// and returns a shared Arc reference (the first one) to the new queue. 
    /// TODO: differentiate between a DFQueueRef consumer and producer!!
    pub fn new_queue(initial_capacity: usize) -> DFQueueRef<T> {
        DFQueueRef {
            qref: Arc::new( 
                DFQueue {
                    queue: Mutex::new(VecDeque::with_capacity(initial_capacity)),
                }
            ),
            local_backlog: VecDeque::new(),
        }
    }

    /// Call this to obtain another reference to the DFQueue. 
    /// There should be one reference per consumer and one refernce per producer.
    /// DFQueueRef does not implement the standard Clone trait, to avoid accidentally cloning it implicity. 
    /// Do not create multiple cloned `DFQueueRef`s for a single producer or consumer entity. 
    pub fn obtain_clone(&self) -> DFQueueRef<T> {
        DFQueueRef {
            qref: self.qref.clone(),
            local_backlog: VecDeque::new(), // TODO: may need to wrap this in an Rc<>
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


    /// internal function that returns the first non-completed item in the queue
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






// Conditionally compile the module `test` only when the test-suite is run.
#[cfg(test)]
mod test {
    
    use std::thread; 
    use super::*;

    #[test]
    // #[should_panic]
    fn simple_test() {


        let mut queue_prod: DFQueueRef<usize> = DFQueueRef::new_queue(8);

        let mut queue_cons = queue_prod.obtain_clone();

        thread::spawn( move || {
            let original_data: Vec<Arc<usize>> = vec![Arc::new(1), Arc::new(2), Arc::new(3), Arc::new(4), Arc::new(5)];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod.enqueue_lockless(elem.clone());
                }

                let queued_data: Arc<usize> = Arc::new(255);
                queue_prod.enqueue_lockless(queued_data);
            }

            loop { }
        });

        thread::spawn( move || {
            while true {
                let mut val = queue_cons.peek_locking();
                if let Some(v) = val {
                    println!("peeked: {:?}", v);
                    v.mark_completed();
                }

            }
        });

        loop { }
    }
}