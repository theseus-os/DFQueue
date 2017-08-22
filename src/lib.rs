//! DFQ is a decoupled, fault-tolerant, multi-producer single-consumer queue.
//! DFQ is compatible with `no_std` and is interrupt-safe by being entirely lock-free and mostly wait-free.
//!
//! DFQ accepts immutable data items only and does not allow the consumer
//! to immediately remove (pop) or modify the items in the queue.
//! In a transactional fashion, the consumer must call [`mark_completed`]: #method.mark_completed 
//! on a queued item to indicate that that item can be safely removed from the queue. 
//! Because the original producer that enqueued that item retains a reference to that item,
//! it is then safe for any entity (any producer or the consumer) to remove it from the queue. 
//! Currently, the consumer simply removes completed items from the queue on the next occasion 
//! of a conumser method like `peek()`.
//!
//! Each producer retains ownership of the data items it queues, and only those items. 
//! Thus, a producer is able to query the status of that item's handling by the consumer, 
//! to see if it is still on the queue or if something has gone wrong and it has failed. 
//! If a failure has occurred, that producer can enqueue that item again, 
//! but of course its original position in the queue will be lost. 


// #![no_std]

#![allow(dead_code)]
#![feature(alloc, collections)]
#![feature(box_syntax)]
#![feature(optin_builtin_traits)] // for negative traits


// #[cfg(test)]
// #[macro_use] extern crate std;
#[cfg(test)]
extern crate time;
#[cfg(test)]
extern crate spin;

extern crate alloc;
extern crate collections;

extern crate core;


mod mpsc_queue;

use core::sync::atomic::{AtomicBool, Ordering};
use core::ptr;
use core::ops::Deref;
use alloc::arc::Arc;





/// The actual queue, an opaque type that cannot be used directly. 
/// The user must use `DFQueueConsumer` and `DFQueueProducer`. 
#[derive(Debug)]
pub struct DFQueue<T> {
    /// the actual inner queue
    queue: MpscQueue<T>,
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
            queue: MpscQueue::new(),
            has_consumer: AtomicBool::default(),
        }
    }


    /// Consumes the DFQueue and returns the one and only consumer for this DFQueue. 
    /// It consumes the DFQueue instance because there is only one consumer allowed per DFQueue.
    pub fn into_consumer(self) -> DFQueueConsumer<T> {
        debug_assert!(self.has_consumer.load(Ordering::SeqCst) == false, 
                      "DFQueue::into_consumer(): FATAL ERROR: already had a consumer!");
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
/// because we do not permit the consumer to directly remove items from the queue.
/// Instead, we require that an element can only be removed from the queue once it has been marked complete,
/// providing a sort of transactional behavior. 
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

    /// Returns the next non-completed element in the queue without actually removing it from the queue,
    /// or `None` if the queue is empty or in a temporarily-inconsistent state. 
    pub fn peek(&self) -> Option<PeekedData<T>> {
        let peek_result = self.qref.queue.peek();
        match peek_result {
            PeekResult::Data(data) => Some(data),
            PeekResult::Empty | PeekResult::Inconsistent => None,
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


    /// Pushes the given `data` onto the queue.
    ///
    /// # Returns 
    /// Returns a QueuedData instance, an Arc-like reference to the given `data` on the queue.
    /// This ensures that the producer can still retain the given `data` if the queue experiences a failure. 
    pub fn enqueue(&self, data: T) -> QueuedData<T>{
        self.qref.queue.push(data)
    }
}




// ---------------------------------------------------------
//    Below is the modified MPSC queue from Rust's libstd
// ---------------------------------------------------------

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::AtomicPtr;


// /// A result of the `pop` function.
// pub enum PopResult<T> {
//     /// Some data has been popped
//     Data(QueuedData<T>),
//     /// The queue is empty
//     Empty,
//     /// The queue is in an inconsistent state. Popping data should succeed, but
//     /// some pushers have yet to make enough progress in order allow a pop to
//     /// succeed. It is recommended that a pop() occur "in the near future" in
//     /// order to see if the sender has made progress or not
//     Inconsistent,
// }


/// A result of the `peek` function.
pub enum PeekResult<T> {
    /// Some data has been successfully peeked
    Data(PeekedData<T>),
    /// The queue is empty
    Empty,
    /// The queue is in an inconsistent state. Peeking data should succeed, but
    /// some pushers have yet to make enough progress in order allow a peek to
    /// succeed. It is recommended that a peek() occur "in the near future" in
    /// order to see if the sender has made progress or not.
    Inconsistent,
}

struct Node<T> {
    /// next points to the node before it, the next one to be popped
    next: AtomicPtr<Node<T>>,
    /// marking the value as None is how we denote an item is removed from the queue
    value: Option<QueuedData<T>>,
    // /// prev points to the node ahead of it, the one that was just pushed in front of it
    // prev: AtomicPtr<Node<T>>, // we don't need prev
}


/// A special reference type that wraps a data item that has been queued. 
/// This is returned to a producer thread (the user of a DFQueueProducer)
/// when enqueuing an item onto the queue so that the producer 
/// can retain a reference to it in the case of failure.
#[derive(Debug)]
pub struct QueuedData<T>(Arc<InnerQueuedData<T>>);
impl<T> QueuedData<T> {

    /// Not public, a DFQueueProducer must call an enqueue function to receive one of these back.
    fn new(data: T) -> QueuedData<T> {
        QueuedData(Arc::new(InnerQueuedData{
            data: data,
            completed: AtomicBool::default(),
        }))
    }


    /// Whether this item has been completed (handled) by the DFQueueConsumer.
    /// If an item is completed, it's okay to remove it from the queue (and it will be removed).
    pub fn is_completed(&self) -> bool {
        self.0.completed.load(Ordering::SeqCst)
    }


    /// Returns true if this data is still on the queue
    /// 
    /// The logic here is as follows: the thread invoking this function holds one reference. 
    /// Thus, if there is more than one reference, then it means it is on the queue, 
    /// because the queue also holds a reference to it. 
    /// That's why we cannot call it internally -- it won't be correct because the original producer thread
    /// may also be holding a reference to it, in order for that producer to retain a reference to it.
    ///
    /// Private note: do not call this internally! This is a public API meant for the producer thread to use.
    pub fn is_enqueued(&self) -> bool {
        Arc::strong_count(&self.0) > 1
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
        (Arc::strong_count(&self.0) == 1)
    }


    /// not public, because we want to control when this is cloned 
    /// so we can keep track of the number of Arc references to the InnerQueuedData.
    fn clone(&self) -> QueuedData<T> {
        QueuedData(self.0.clone())
    }

    
    /// creates a clone of this, marshalled as a PeekedData 
    /// so that a consumer can access the InnerQueuedData without being able to access 
    /// all of the QueuedData methods here.
    fn as_peeked(&self) -> PeekedData<T> {
        PeekedData(self.0.clone())
    }
}

impl<T> Deref for QueuedData<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.data
    }
}


/// A wrapper around data in the queue that allows a DFQueueConsumer 
/// to access the data and mark the queued item as completed. 
/// Automatically Derefs to the inner type `&T`, just like Arc does. 
#[derive(Debug)]
pub struct PeekedData<T>(Arc<InnerQueuedData<T>>);
impl<T> PeekedData<T> {
    pub fn mark_completed(&self) {
        self.0.completed.store(true, Ordering::Release);
    }
}
impl<T> Deref for PeekedData<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.data
    }
}


#[derive(Debug)]
struct InnerQueuedData<T> {
    /// the actual value contained in the Node
    data: T,
    /// whether this queued item has been completed
    completed: AtomicBool,
}


/// The multi-producer single-consumer structure. This is not cloneable, but it
/// may be safely shared so long as it is guaranteed that there is only one
/// popper at a time (many pushers are allowed).
/// Borrowed from the Rust libstd's mpsc_queue, but modified slightly.
/// Therefore, we assume it's safe. We also preserve safety by 
/// always checking for null pointers before dereferencing.
#[derive(Debug)]
struct MpscQueue<T> {
    /// The head is the node that was most recently pushed
    head: AtomicPtr<Node<T>>,
    /// the tail is a placeholder that exists right after the one that was pushed first.
    /// The tail's next pointer points to the next Node to be popped off.
    tail: UnsafeCell<*mut Node<T>>,
}

unsafe impl<T: Send> Send for MpscQueue<T> { }
unsafe impl<T: Send> Sync for MpscQueue<T> { }

impl<T> Node<T> {
    unsafe fn new(v: Option<QueuedData<T>>) -> *mut Node<T> {
        Box::into_raw(box Node {
            next: AtomicPtr::new(ptr::null_mut()),
            // prev: AtomicPtr::new(ptr::null_mut()), // we don't need prev
            value: v,
        })
    }
}

impl<T> MpscQueue<T> {
    /// Creates a new queue that is safe to share among multiple producers and one consumer. 
    /// Its storage semantics are push onto the front, pop off the back. 
    fn new() -> MpscQueue<T> {
        let stub = unsafe { Node::new(None) };
        MpscQueue {
            head: AtomicPtr::new(stub),
            tail: UnsafeCell::new(stub),
        }
    }

    /// Pushes a new value onto the front of this queue.
    fn push(&self, t: T) -> QueuedData<T> {
        let queued_data = QueuedData::new(t);

        unsafe {
            let n = Node::new(Some(queued_data.clone())); // Kevin: 'n' will be the new head
            let prev = self.head.swap(n, Ordering::AcqRel); // here, prev becomes the original head
            // here, prev = old head, and head = n
            // (*n).prev.store(prev, Ordering::Release); // KevinBoos: current head (n)'s prev = old head (prev) // we don't need prev
            (*prev).next.store(n, Ordering::Release);  // prev(old head).next = n
        }

        queued_data
    }


    // We don't permit popping from the DFQueue, so this is currently disabled. 
    // It's also disabled becausing using both peek() and pop() do not play well together,
    // because pop() has no understanding of whether a node is completed. 
    // /// Pops some data from the back of the queue. 
    // ///
    // /// Note that the current implementation means that this function cannot
    // /// return `Option<T>`. It is possible for this queue to be in an
    // /// inconsistent state where many pushes have succeeded and completely
    // /// finished, but pops cannot return `Some(t)`. This inconsistent state
    // /// happens when a pusher is pre-empted at an inopportune moment.
    // ///
    // /// This inconsistent state means that this queue does indeed have data, but
    // /// it does not currently have access to it at this time.
    // fn pop(&self) -> PopResult<T> {
    //     unsafe {
    //         let tail = *self.tail.get();
    //         let next = (*tail).next.load(Ordering::Acquire);

    //         if !next.is_null() {
    //             *self.tail.get() = next;
    //             assert!((*tail).value.is_none());
    //             assert!((*next).value.is_some());
    //             let ret = (*next).value.take().unwrap();
    //             let _: Box<Node<T>> = Box::from_raw(tail);
    //             return PopResult::Data(ret);
    //         }

    //         if self.head.load(Ordering::Acquire) == tail {PopResult::Empty} else {PopResult::Inconsistent}
    //     }
    // }


    /// Peeks at this queue and returns a reference to the next non-completed item.
    ///
    /// It is possible for this queue to be in an inconsistent state 
    /// where many pushes have succeeded and completely finished, 
    /// but a peek cannot return some queued data. This inconsistent state happens 
    //// when a pusher is pre-empted at an inopportune moment before completing its push.
    ///
    /// An Inconsistent state means that this queue does indeed have data, but
    /// it does not currently have access to it at this time.
    /// If Inconsistent is returned, try to peek again after a short wait.
    fn peek(&self) -> PeekResult<T> {
        unsafe {
            let mut tail = *self.tail.get();
            let mut next = (*tail).next.load(Ordering::Acquire);

            while !next.is_null() {
                assert!((*next).value.is_some()); // all nodes still in the queue should have Some values (not None)

                // if the node has been completed, we no longer want to return it when peeking
                if (*next).value.as_ref().unwrap().is_completed() {

                    // just go ahead and remove the completed node
                    // to remove a node, we point "self.tail" to it and then remove its value
                    *self.tail.get() = next;
                    assert!((*tail).value.is_none());
                    let _ = (*next).value.take().unwrap(); // remove the value and discard it
                    let _: Box<Node<T>> = Box::from_raw(tail); // grant ownership of the original tail node and let it fall out of scope
                    
                    // move to the next node
                    tail = *self.tail.get();
                    next = (*tail).next.load(Ordering::Acquire);
                    continue;
                }

                assert!((*tail).value.is_none());
                assert!((*next).value.is_some());
                let ret = (*next).value.as_ref().unwrap().as_peeked();
                return PeekResult::Data(ret);
            }

            if self.head.load(Ordering::Acquire) == tail {PeekResult::Empty} else {PeekResult::Inconsistent}
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















// Conditionally compile the module `test` only when the test-suite is run.
#[cfg(test)]
mod test {
    


    use std::sync::{Arc, Barrier};
    use std::thread;

    use time::{Duration, PreciseTime};
    
    use std::vec::Vec;
    use collections::VecDeque;
    use super::*;

    use spin::Mutex;



    // #[test]
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
                    queue_prod.enqueue(*elem);
                }
            }
            
            let queued_data = queue_prod.enqueue(256);
            println!("prod1: queued_data = {:?}", queued_data);

        } );

         let mut thr_p2 = thread::spawn( move || {
            let original_data: Vec<usize> = vec![10, 11, 12, 13, 14];

            for i in 1..20 {
                for elem in original_data.iter() {
                    queue_prod2.enqueue(*elem);
                }
            }

            let queued_data = queue_prod2.enqueue(512);
            println!("prod2: queued_data = {:?}", queued_data);
        } );

        let mut thr_c = thread::spawn( move || {
            loop {
                let mut val = queue_cons.peek();
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



    // #[test]
    // fn mpsc_queue_test() {
    //     let nthreads = 16;
    //     let top_range = 10;
    //     let q = MpscQueue::new();
    //     match q.peek() {
    //         PeekResult::Empty => {}
    //         PeekResult::Inconsistent | PeekResult::Data(..) => panic!()
    //     }
        
    //     let q = Arc::new(q);
    //     let mut threads = vec![];

    //     for id in 0..nthreads {
    //         let q = q.clone();
    //         threads.push(thread::spawn(move|| {
    //             for i in 0..top_range {
    //                 let push_val = i + (id * top_range);
    //                 println!("{}", push_val);
    //                 q.push(push_val);
    //                 for y in 0..1 {
    //                     let mut a = y + 10;
    //                     a += 1;
    //                 }
    //             }
    //         }));
    //     }

    //     for t in threads {
    //         t.join().unwrap();
    //     }

    //     // unsafe {
    //     //     let mut curr = q.tail.load(Ordering::SeqCst);
    //     //     let curr_node = &*curr;
    //     //     println!("ail Node: val={:?}, ", curr_node.value);
    //     //     // if next_node_ptr.is_null() { 
    //     //     //     print!("next=NULL, ");
    //     //     // } else { 
    //     //     //     print!("next={:?}, ", (*next_node_ptr).value);
    //     //     // }


    //     //     while !(*curr).next.load(Ordering::SeqCst).is_null()
    //     //     {
    //     //         let curr_node = &*curr;
    //     //         let next_node_ptr = (*curr).next.load(Ordering::SeqCst);

    //     //         print!("Node: val={:?}, ", curr_node.value);
    //     //         if next_node_ptr.is_null() { 
    //     //             print!("next=NULL, ");
    //     //         } else { 
    //     //             print!("next={:?}, ", (*next_node_ptr).value);
    //     //         }

    //     //         curr = (*curr).prev.load(Ordering::SeqCst);
    //     //     }
    //     // }

    //     let mut i = 0;
    //     // while i < nthreads * top_range {
    //     loop {
    //         let peeked = q.peek();
    //         match peeked {
    //             PeekResult::Empty | PeekResult::Inconsistent => {
    //                 // println!("peeked data None");
    //             },
    //             PeekResult::Data(x) => { 
    //                 // i += 1;
    //                 println!("peeked data {}", &*x);
    //                 x.mark_completed();
    //             }
    //         }
    //     }

    //     println!("done!");
    // }




    const NUM_ELEMENTS: u64 = 1048576*32; // must be a power of 2 above 32
    // const NUM_ELEMENTS: u64 = 16; // must be a power of 2 above 32
    const NUM_TRIALS: u64 = 10;


    #[test]
    fn bench_dfqueue() {
        println!("========== DFQueue ========== ");
        

        for i in [1, 2, 4, 8, 16].iter() {
            let mut results: Vec<u64> = Vec::with_capacity(NUM_TRIALS as usize);
            for t in 0..NUM_TRIALS {
                results.push(run_dfqueue(i.clone(), NUM_ELEMENTS).num_nanoseconds().unwrap() as u64);
            }


            results.sort();
            let median = results[results.len()/2];
            // let average = results.iter().fold(0, |a, &b| a + b) / NUM_TRIALS;
            println!("producers={}, elements={}, elapsed={:?}", i, NUM_ELEMENTS, median);
        }
    }



    fn run_dfqueue(producers: u64, elements: u64) -> Duration {
        // println!("running with producers={}, elements={}", producers, elements);

        let queue: DFQueue<u64> = DFQueue::new();
        let mut queue_prod = queue.into_producer();
        let mut queue_cons = queue_prod.get_consumer();
        let qcons = queue_cons.unwrap();


        // start time
        let start = PreciseTime::now();

        // consumer
        let consumer_thread = thread::spawn(move|| {
            
            let mut total_received = 0;
            while (total_received < elements) {
                let mut val = qcons.peek();
                if let Some(v) = val {
                    v.mark_completed();
                    total_received += 1;
                }
            }        
        });

        let barrier = Arc::new(Barrier::new(producers as usize));
        // let mut threads = vec![];

        // producers
        for _ in 0..producers {
            let q = queue_prod.obtain_producer();
            let c = barrier.clone();

            // threads.push(
            thread::spawn(move|| {
                c.wait();
                for i in 0..(elements/producers) {
                    q.enqueue(i);
                }
            });
        }


        consumer_thread.join();
        let end = PreciseTime::now(); 
        
        // for t in threads {
        //     t.join().unwrap();
        // }

        start.to(end)       
    }





















    #[test]
    fn bench_rust_mutex_queue() {
        println!("========== Standard Rust Mutex VecDeque queue ========== ");

        for i in [1, 2, 4, 8, 16].iter() {
            let mut results: Vec<u64> = Vec::with_capacity(NUM_TRIALS as usize);
            for t in 0..NUM_TRIALS {
                results.push(run_mpsc_queue(i.clone(), NUM_ELEMENTS).num_nanoseconds().unwrap() as u64);
            }

            results.sort();
            let median = results[results.len()/2];
            // let average = results.iter().fold(0, |a, &b| a + b) / NUM_TRIALS;
            println!("producers={}, elements={}, elapsed={:?}", i, NUM_ELEMENTS, median);
        }
    }



    fn run_mutex_queue(producers: u64, elements: u64) -> Duration {
        // println!("running with producers={}, elements={}", producers, elements);

        let queue : Mutex<VecDeque<u64>> = Mutex::new(VecDeque::new());
        let qref = Arc::new(queue);

        // start time
        let start = PreciseTime::now();

        // consumer
        let qcons = qref.clone();
        let consumer_thread = thread::spawn(move|| {
            
            let mut total_received = 0;
            while (total_received < elements) {
                match qcons.lock().pop_front() {
                    Some(_) => { total_received += 1; }
                    _ => { }
                }
            }        
        });


        // let mut threads = vec![];
        let barrier = Arc::new(Barrier::new(producers as usize));

        // producers
        for _ in 0..producers {
            let q = qref.clone();
            let c = barrier.clone();

            // producer_threads.push(
            thread::spawn(move|| {
                c.wait();
                for i in 0..(elements/producers) {
                    q.lock().push_back(i);
                }
            });
            // );
        }

        // for t in threads {
        //     t.join().unwrap();
        // }

        consumer_thread.join();
        let end = PreciseTime::now(); 
        
        start.to(end)       
    }








    use super::mpsc_queue::{Queue, Data, Empty, PopResult, Inconsistent};




    #[test]
    fn bench_rust_mpsc_queue() {
        println!("========== Standard Rust MPSC lockless queue ========== ");

        for i in [1, 2, 4, 8, 16].iter() {
            let mut results: Vec<u64> = Vec::with_capacity(NUM_TRIALS as usize);
            for t in 0..NUM_TRIALS {
                results.push(run_mpsc_queue(i.clone(), NUM_ELEMENTS).num_nanoseconds().unwrap() as u64);
            }

            results.sort();
            let median = results[results.len()/2];
            // let average = results.iter().fold(0, |a, &b| a + b) / NUM_TRIALS;
            println!("producers={}, elements={}, elapsed={:?}", i, NUM_ELEMENTS, median);
        }
    }



    fn run_mpsc_queue(producers: u64, elements: u64) -> Duration {
        // println!("running with producers={}, elements={}", producers, elements);

        let queue = Queue::new();
        let qref = Arc::new(queue);

        // start time
        let start = PreciseTime::now();

        // consumer
        let qcons = qref.clone();
        let consumer_thread = thread::spawn(move|| {
            
            let mut total_received = 0;
            while (total_received < elements) {
                match qcons.pop() {
                    PopResult::Data(_) => { total_received += 1}
                    _ => { }
                }
            }        
        });


        // let mut threads = vec![];
        let barrier = Arc::new(Barrier::new(producers as usize));

        // producers
        for _ in 0..producers {
            let q = qref.clone();
            let c = barrier.clone();

            // producer_threads.push(
            thread::spawn(move|| {
                c.wait();
                for i in 0..(elements/producers) {
                    q.push(i);
                }
            });
            // );
        }

        // for t in threads {
        //     t.join().unwrap();
        // }

        consumer_thread.join();
        let end = PreciseTime::now(); 
        
        start.to(end)       
    }
}