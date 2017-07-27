//! DFQ is a decoupled, fault-tolerant, multi-producer single-consumer queue.
//! DFQ is compatible with `no_std` and is optionally interrupt-safe by being "temporarily lockless".
//!
//! DFQ accepts immutable data items only and does not allow the consumer
//! to immediately remove (pop) or modify the items in the queue.
//! In a transactional fashion, the consumer must call [`mark_completed`]: #method.mark_completed 
//! on a queued item to indicate that that item can be safely removed from the queue. 
//! Because the original producer that enqueued that item retains a reference to that item,
//! it is then safe for any entity (any producer or the consumer) to remove it from the queue. 
//!
//! Each producer retains ownership of the data items it queues, and only those items. 
//! Thus, a producer is able to query the status of that item's handling by the consumer, 
//! to see if it is still on the queue or if something has gone wrong and it has failed. 
//! If a failure has occurred, that producer can enqueue that item again. 

#![no_std]

#![allow(dead_code)]
#![feature(alloc, collections)]
#![feature(box_syntax)]


extern crate alloc;
extern crate collections;

use core::sync::atomic::{AtomicBool, Ordering};
use core::ops::Deref;
use alloc::arc::Arc;
use collections::VecDeque;


mod mpsc_queue;

use mpsc_queue::Queue;



/// Defines the policy for removing completed items on the queue for a given function call. 
/// If nothing is specified, the default for a `_locking` function is `RemoveNow`, 
/// while the default for a `_lockless` function is `NoRemoval`.
pub enum RemovalPolicy {
    
    /// Ensures that this call will NOT perform any removal of completed items.
    /// Good for ensuring fastest execution. 
    NoRemoval,

    /// Asks this call to attempt to remove all completed items right now. 
    /// Of course, if the inner queue cannot be locked, then no elements will be removed.
    RemoveNow,

    // /// Attempts to remove completed items every `N` invocations of this call, 
    // /// in which `N` is the given period. 
    // Periodic(usize),
}


// fn remove_completed_items<T>(locked_queue: &mut MutexGuard<VecDeque<QueuedData<T>>>) {
//     // retain elements that are not completed
//     locked_queue.retain(|x| !x.is_completed());
// }



/// A special reference type that wraps a data item that has been queued. 
/// This is returned to a producer thread (the user of a DFQueueProducer)
/// when enqueuing an item onto the queue so that the producer 
/// can retain a reference to it in the case of failure.
#[derive(Debug)]
pub struct QueuedData<T> {
    inner: Arc<InnerQueuedData<T>>,
}

impl<T> QueuedData<T> {

    /// Not public, a DFQueueProducer must call an enqueue function to receive one of these back.
    fn new(data: T) -> QueuedData<T> {
        QueuedData {
            inner: Arc::new(InnerQueuedData::new(data)),
        }
    }

    /// Whether this item has been completed (handled) by the DFQueueConsumer.
    /// If an item is completed, the producer knows it's okay to remove it from the queue.
    pub fn is_completed(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }


    /// Returns true if this data is still on the queue
    /// 
    /// The logic here is as follows: the thread invoking this function holds one reference. 
    /// Thus, if there is more than one reference, then it means it is on the queue, 
    /// because the queue also holds a reference to it. 
    /// That's why we cannot call it internally --  it won't be correct because the original producer thread
    /// may also be holding a reference to it, in order for that producer to retain a reference to it 
    //// 
    ///
    /// Private note: do not call this internally! This is a public API meant for the producer thread to use.
    pub fn is_enqueued(&self) -> bool {
        Arc::strong_count(&self.inner) > 1
    }


    /// The logic here is as follows: if the item on the queue has only one reference, and has not been completed,
    /// that means it is no longer on the queue (the consumer thread crashed or failed). 
    /// Again, the producer thread invoking this function holds one reference, and if the data is indeed enqueued, 
    /// that constitutes another reference. 
    /// If the reference held in the queue isn't there (only one total), and the thread hasn't completed 
    /// (it may have just been completed in another enqueue call), then something went wrong because it's not complete
    /// and not on the queue anymore. 
    /// 
    /// Private note: do not call this internally! This is a public API meant for the producer thread to use.
    pub fn has_failed(&self) -> bool {
        // an item that has been completed could not have possibly failed!
        (!self.is_completed()) && 
        (Arc::strong_count(&self.inner) == 1)
    }


    /// creates a clone of this, marshalled as a PeekedData 
    /// so that a consumer can access the InnerQueuedData without being able to access 
    /// all of the QueuedData methods here.
    fn as_peeked(&self) -> PeekedData<T> {
        PeekedData {
            inner: self.inner.clone(),
        }
    }

    /// not public, because we want to control when this is cloned 
    /// so we can keep track of the number of Arc references to the InnerQueuedData.
    fn clone(&self) -> QueuedData<T> {
        QueuedData {
            inner: self.inner.clone(),
        }
    }
}



#[derive(Debug)]
struct InnerQueuedData<T> {
    data: T,
    completed: AtomicBool, 
}

impl<T> InnerQueuedData<T> {

    fn new(data: T) -> InnerQueuedData<T> {
        InnerQueuedData {
            data: data,
            completed: AtomicBool::new(false),
        }
    }

}


/// A wrapper around data in the queue that allows a DFQueueConsumer 
/// to access the data and mark the queued item as completed. 
/// Automatically Derefs to the inner type `&T`, just like Arc does. 
#[derive(Debug)]
pub struct PeekedData<T> {
    inner: Arc<InnerQueuedData<T>>,
}

impl<T> PeekedData<T> {

    /// Mark this item as completed.
    /// This allows the producer (the one that pushed it onto the queue) to know that it's safe to remove it from the queue.
    pub fn mark_completed(&self) {
        self.inner.completed.store(true, Ordering::SeqCst);
    }

}

impl<T> Deref for PeekedData<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner.data
    }
}


/// The actual queue, an opaque type that cannot be used directly. 
/// The user must use `DFQueueConsumer` and `DFQueueProducer`. 
#[derive(Debug)]
pub struct DFQueue<T> {
    /// the actual inner queue
    queue: Queue<QueuedData<T>>,
    /// whether this queue has a consumer (it can only have one!!)
    has_consumer: AtomicBool,
}

impl<T> DFQueue<T> {

    /// Creates a new DFQueue. 
    ///
    /// This object cannot be used directly, you must obtain a producer or consumer to the queue
    /// using the functions `into_consumer()` or `obtain_producer()`.
    pub fn new() -> DFQueue<T> {
        DFQueue {
            queue: Queue::new(),
            has_consumer: AtomicBool::default(),
        }
    }


    /// Consumes the DFQueue and returns the one and only consumer for this DFQueue. 
    /// It consumes the DFQueue instance because there is only one consumer allowed per DFQueue.
    pub fn into_consumer(self) -> DFQueueConsumer<T> {
        debug_assert!(self.has_consumer.load(Ordering::SeqCst) == false, 
                      "DFQueue::into_consumer(): FATAL ERROR: already had a consumer!");
        // TODO: fix this with cmp & Swap or cmpexchg
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

    /// Returns a new DFQueueProducer cloned from this consumer instance, since there can be multiple producers.
    pub fn obtain_producer(&self) -> DFQueueProducer<T> {
        DFQueueProducer {
            qref: self.qref.clone(),
        }
    }



    /// Peeks at the queue (well, currently just pops it immediately.)
    ///
    /// If no `RemovalPolicy` is given, the default for a locking function is `RemoveNow`.
    ///
    /// Returns the first non-completed element in the queue without actually removing it from the queue (TODO!!!), 
    /// or `None` if the queue is empty. 
    pub fn peek(&self, policy: Option<RemovalPolicy>) -> Option<PeekedData<T>> {
        use mpsc_queue::PopResult;

        let pop_result = self.qref.queue.pop();
        match pop_result {
            PopResult::Data(data) => Some(data.as_peeked()),
            PopResult::Empty | PopResult::Inconsistent => None,
        }

    }


}


/// A producer that can enqueue elements into a DFQueue.
/// Do not wrap this in an Arc or Mutex, the queue it is already protected by those on the interior. 
#[derive(Debug)]
pub struct DFQueueProducer<T> {
    qref: Arc<DFQueue<T>>,
}


impl<T> DFQueueProducer<T> {

    /// Call this to obtain another DFQueueProducer. 
    /// DFQueueProducer does not implement the standard Clone trait, to avoid accidentally cloning it implicity. 
    pub fn obtain_producer(&self) -> DFQueueProducer<T> {
        DFQueueProducer {
            qref: self.qref.clone(),
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


    /// Pushes the given `data` onto the back of the queue.
    ///
    /// If no `RemovalPolicy` is given, the default for a locking function is `RemoveNow`.
    ///
    /// # Returns 
    /// Returns a QueuedData instance, an Arc-like reference to the given `data` on the queue.
    /// This ensures that the producer can still retain the given `data` if the queue experiences a failure. 
    ///
    pub fn enqueue(&self, data: T, policy: Option<RemovalPolicy>) -> QueuedData<T>{

        let queued_data: QueuedData<T> = QueuedData::new(data);
        self.qref.queue.push(queued_data.clone());

        queued_data
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
            let original_data: Vec<usize> = vec![1, 2, 3, 4, 5];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod.enqueue(*elem, None);
                }
            }
            
            let (queued_data, backlogged) = queue_prod.enqueue(256, None);
            println!("prod1: queued_data = {:?}, backlogged?: {}", queued_data, backlogged);

            queue_prod.flush_backlog();
        } );

         let mut thr_p2 = thread::spawn( move || {
            let original_data: Vec<usize> = vec![10, 11, 12, 13, 14];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod2.enqueue(*elem, None);
                }
            }

            let (queued_data, backlogged) = queue_prod2.enqueue(512, None);
            println!("prod2: queued_data = {:?}, backlogged?: {}", queued_data, backlogged);

            queue_prod2.flush_backlog();
        } );

        let mut thr_c = thread::spawn( move || {
            loop {
                let mut val = queue_cons.peek(Some(RemovalPolicy::RemoveNow));
                if let Some(v) = val {
                    println!("peeked: {:?}, value={}", v, *v);
                    v.mark_completed();
                }
                else {
                    // println!("\nDumping queue: \n  {:?}", queue_cons.qref.queue);
                }
            }
        } );

        thr_p.join();
        println!("producer 1 done");
        thr_p2.join();
        println!("producer 2 done");
        thr_c.join();
    }
}