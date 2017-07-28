# DFQueue

DFQ is a **D**ecoupled, **F**ault-tolerant **Queue** that permits multiple producers and a single consumer.
DFQ is compatible with `no_std` and is interrupt-safe by being entirely lock-free and mostly wait-free.

DFQ accepts immutable data items only and does not allow the consumer
to immediately remove (pop) or modify the items in the queue.
Instead, DFQ imposes a transactional nature on the consumer, requiring it to `peek()` into the queue
and call `mark_completed()`on a queued item to indicate that that item can be safely removed from the queue. 

The original producer that enqueued that item can optionally retain a reference to that item (more specifically, shared ownership of that item with the queue via `Arc`) to that item. 
This enables any entity (any producer or the consumer) to remove a completed item from the queue safely. 
Currently, the consumer simply removes completed items from the queue on the next invocation of `peek()`.

Because each producer retains ownership of the data items it queues, a producer can query the status of that item's handling by the consumer to see if it is still on the queue or if something has gone wrong and it has failed. 
If a failure has occurred, that producer can enqueue that item again, but of course its original position in the queue will be lost. 

## Example Usage 

See the `simple_test()` test function at the bottom of `src/lib.rs` for an example of how to use DFQueue. 

## Inspiration

DFQueue's underlying queue is based on the Rust libstd's mpsc_queue, which itself is based on this [non-intrusive MPSC node-based queue](http://www.1024cores.net/home/lock-free-algorithms/queues/non-intrusive-mpsc-node-based-queue), which offers lock- and wait-freedom suitable for usage in interrupt handlers. 

We have augmented it to remove the pop functionality, which is a non-recoverable operation, in favor of peeking into the queue. 

Below is the original MPSC queue algorithm implemented in C, copied from [here](http://www.1024cores.net/home/lock-free-algorithms/queues/non-intrusive-mpsc-node-based-queue) for posterity.
```
struct mpscq_node_t 
{ 
    mpscq_node_t* volatile  next; 
    void*                   state; 

}; 

struct mpscq_t 
{ 
    mpscq_node_t* volatile  head; 
    mpscq_node_t*           tail; 
}; 

void mpscq_create(mpscq_t* self, mpscq_node_t* stub) 
{ 
    stub->next = 0; 
    self->head = stub; 
    self->tail = stub; 
} 

void mpscq_push(mpscq_t* self, mpscq_node_t* n) 
{ 
    n->next = 0; 
    mpscq_node_t* prev = XCHG(&self->head, n); // serialization-point wrt producers, acquire-release
    prev->next = n; // serialization-point wrt consumer, release
} 

mpscq_node_t* mpscq_pop(mpscq_t* self) 
{ 
    mpscq_node_t* tail = self->tail; 
    mpscq_node_t* next = tail->next; // serialization-point wrt producers, acquire
    if (next) 
    { 
        self->tail = next; 
        tail->state = next->state; 
        return tail; 
    } 
    return 0; 
} 
```