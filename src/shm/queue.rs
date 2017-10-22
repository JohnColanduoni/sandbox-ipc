use ::{align, CACHE_LINE};
use ::shm::{SharedMem, SharedMemMap};

use std::{io, mem, thread, slice, usize, isize};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::borrow::Borrow;
use std::sync::{LockResult, PoisonError};
use std::sync::atomic::{Ordering, AtomicUsize, AtomicBool};

use uuid::Uuid;

#[derive(Debug)]
pub struct Queue<B = SharedMemMap<SharedMem>, C = SharedMem> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    mem: B,
    raw_offset: usize,
    control: *const SharedMemQueueCtrl,

    items_base: *mut u8,
    item_size: usize,
    item_offset: usize,
    item_count: usize,

    _phantom: PhantomData<C>,
}

unsafe impl<B, C> Send for Queue<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{ }

unsafe impl<B, C> Sync for Queue<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{ }

#[derive(Serialize, Deserialize, Debug)]
pub struct Handle {
    raw_offset: usize,
    item_size: usize,
    item_count: usize,
    shm_token: Uuid,
}

#[repr(C)]
struct SharedMemQueueCtrl {
    next_send_index: AtomicUsize,
    last_sent_index: AtomicUsize,
    next_recv_index: AtomicUsize,
    last_recvd_index: AtomicUsize,
    poison: AtomicBool,
}

impl Queue<SharedMemMap<SharedMem>, SharedMem> {
    pub fn required_size(item_size: usize, item_count: usize) -> usize {
        assert!(item_count > 0, "item count must be positive");
        assert!(item_size > 0, "item size must be positive");
        assert!(item_count < (isize::MAX as usize), "implementation cannot handle buffer sizes >= isize::MAX");
        let item_offset = Self::item_offset(item_size);
        item_offset * item_count + mem::size_of::<SharedMemQueueCtrl>()
    }
}

// The queue is laid out in memory as a sequence of items aligned to 
// cache lines, followed by
impl<B, C> Queue<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub unsafe fn new_with_memory(item_size: usize, item_count: usize, memory: B, offset: usize) -> io::Result<Self> {
        let required_size = Queue::required_size(item_size, item_count);
        if memory.borrow().len() < offset + required_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "insufficient space for shared memory queue of requested size"));
        }
        if (memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "mapped memory for shared memory queue must be aligned"));
        }
        let raw_offset = memory.borrow().offset() + offset;

        let item_offset = Self::item_offset(item_size);
        let items_base = memory.borrow().pointer().offset(offset as isize);
        let control = items_base.offset((item_count * item_offset) as isize) as *mut SharedMemQueueCtrl;
        *control = SharedMemQueueCtrl {
            next_send_index: AtomicUsize::new(0),
            last_sent_index: AtomicUsize::new(usize::MAX),
            next_recv_index: AtomicUsize::new(0),
            last_recvd_index: AtomicUsize::new(usize::MAX),
            poison: AtomicBool::new(false),
        };

        Ok(Queue {
            mem: memory,
            raw_offset,
            control: control as *const SharedMemQueueCtrl,

            items_base,
            item_size,
            item_offset,
            item_count,

            _phantom: PhantomData,
        })
    }

    pub fn from_handle(handle: Handle, memory: B) -> io::Result<Self> {
        if handle.shm_token != memory.borrow().token() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the queue is not associated with the given shared memory"));
        }
        if !(handle.raw_offset <= memory.borrow().offset() &&
                handle.raw_offset + Queue::required_size(handle.item_size, handle.item_count) <= memory.borrow().offset() + memory.borrow().len())
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the queue is not contained within this shared memory mapping"));
        }
        let local_offset = handle.raw_offset - memory.borrow().offset();
        let item_offset = Self::item_offset(handle.item_size);
        let items_base;
        let control;
        unsafe {
            items_base = memory.borrow().pointer().offset(local_offset as isize);
            control = items_base.offset((handle.item_count * item_offset) as isize) as *const SharedMemQueueCtrl;
        }

        Ok(Queue {
            mem: memory,
            raw_offset: handle.raw_offset,
            control,

            items_base,
            item_size: handle.item_size,
            item_offset,
            item_count: handle.item_count,

            _phantom: PhantomData,
        })
    }

    pub fn handle(&self) -> io::Result<Handle> {
        Ok(Handle {
            raw_offset: self.raw_offset,
            item_size: self.item_size,
            item_count: self.item_count,
            shm_token: self.mem.borrow().token(),
        })
    }

    pub fn try_push(&self) -> Option<PushGuard<B, C>> {
        let control = self.control();
        let send_index = control.next_send_index.fetch_add(1, Ordering::SeqCst);
        if send_index == usize::MAX {
            // Wrap-around
            unimplemented!()
        }
        // Ensure the queue is not full
        'check_send_clear: loop {
            // Although the read index we get may be out of date, it will never decrease (except in a wrap-around situation)
            let last_recvd_index = control.last_recvd_index.load(Ordering::SeqCst);

            if (send_index.wrapping_sub(last_recvd_index.wrapping_add(1)) as isize) < (self.item_count as isize) {
                break 'check_send_clear;
            } else {
                // The queue is full, put back this index if nobody has claimed the index after it
                match control.next_send_index.compare_exchange_weak(send_index.wrapping_add(1), send_index, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return None, // We were never here
                    Err(_) => {
                        // Other producers attempted to write to the full queue in between our obtaining this index and trying
                        // to put it back. To handle this case we repeat the entire full queue check. If the index we own is now
                        // unobstructed, we break the loop and proceed normally. This is acceptable because all other producers will be
                        // attemting the same action, so any holes will be filled (no producers with a lower index will be allowed to vacate
                        // it before we do). If not, we attempt to put the index back. Since all blocked producers will be attempting the 
                        // same action, either the consumer will free space for the producers or the producers will put their indicies 
                        // back one by one (starting with the last one to obtain an index).
                        //
                        // At least one of the producers is guaranteed to make progress unless the platform's CAS is failable. We explicitly
                        // use weak CAS because a failure may allow progress to come from either the consumer or other producers releasing their
                        // indices.

                        thread::yield_now(); // Let other threads run to make progress
                        continue 'check_send_clear;
                    },
                }
            }
        }

        Some(PushGuard {
            queue: self,
            send_index,
            slice: unsafe { slice::from_raw_parts_mut(
                self.items_base.offset(((send_index % self.item_count) * self.item_offset) as isize),
                self.item_size,
            ) },
        })
    }

    pub fn try_pop(&self) -> LockResult<Option<PopGuard<B, C>>> {
        let control = self.control();
        let recv_index = control.next_recv_index.fetch_add(1, Ordering::SeqCst);
        if recv_index == usize::MAX {
            // Wrap-around
            unimplemented!()
        }
        // Ensure the queue is not empty
        'check_recv_clear: loop {
            // Although the write index we get may be out of date, it will never decrease (except in a wrap-around situation)
            let last_sent_index = control.last_sent_index.load(Ordering::SeqCst);

            if last_sent_index.wrapping_sub(recv_index) as isize >= 0 {
                break;
            } else {
                match control.next_recv_index.compare_exchange_weak(recv_index.wrapping_add(1), recv_index, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return Ok(None),
                    Err(_) => {
                        // Other consumers attempted to read from the empty queue in between our obtaining this index and trying
                        // to put it back. To handle this case we repeat the entire empty queue check. If the index we own is now
                        // unobstructed, we break the loop and proceed normally. This is acceptable because all other consumers will be
                        // attemting the same action, so any holes will be filled (no consumers with a lower index will be allowed to vacate
                        // it before we do). If not, we attempt to put the index back. Since all blocked consumers will be attempting the 
                        // same action, either the producer will provide a value for the consumers or the consumers will put their indicies 
                        // back one by one (starting with the last one to obtain an index).
                        //
                        // At least one of the consumers is guaranteed to make progress unless the platform's CAS is failable. We explicitly
                        // use weak CAS because a failure may allow progress to come from either the producer or other consumers releasing their
                        // indices.

                        thread::yield_now();
                        continue 'check_recv_clear;
                    }
                }
            }
        }

        let guard = Some(PopGuard {
            queue: self,
            recv_index,
            slice: unsafe { slice::from_raw_parts(
                self.items_base.offset(((recv_index % self.item_count) * self.item_offset) as isize),
                self.item_size,
            ) },
        });

        if !control.poison.load(Ordering::SeqCst) {
            Ok(guard)
        } else {
            Err(PoisonError::new(guard))
        }
    }

    fn item_offset(item_size: usize) -> usize {
        align(item_size, CACHE_LINE)
    }

    #[inline]
    fn control(&self) -> &SharedMemQueueCtrl {
        unsafe { &(*self.control) }
    }
}

pub struct PushGuard<'a, B: 'a, C: 'a> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    queue: &'a Queue<B, C>,
    send_index: usize,
    slice: &'a mut [u8],
}

impl<'a, B: 'a, C: 'a> Drop for PushGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        let control = self.queue.control();
        if thread::panicking() {
            control.poison.store(true, Ordering::SeqCst);
        }
        // Increment last sent index, but only if all previous sends have completed
        loop {
            match control.last_sent_index.compare_exchange_weak(self.send_index.wrapping_sub(1), self.send_index, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(_) => {
                    thread::yield_now();
                    continue;
                },
            }
        }
    }
}

pub struct PopGuard<'a, B: 'a, C: 'a> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    queue: &'a Queue<B, C>,
    recv_index: usize,
    slice: &'a [u8],
}

impl<'a, B: 'a, C: 'a> Drop for PopGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        let control = self.queue.control();
        if thread::panicking() {
            control.poison.store(true, Ordering::SeqCst);
        }
        // Increment last sent index, but only if all previous sends have completed
        loop {
            match control.last_recvd_index.compare_exchange_weak(self.recv_index.wrapping_sub(1), self.recv_index, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(_) => {
                    thread::yield_now();
                    continue;
                },
            }
        }
    }
}

impl<'a, B, C> Deref for PushGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    type Target = [u8];
    
    #[inline]
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

impl<'a, B, C> DerefMut for PushGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.slice
    }
}

impl<'a, B, C> Deref for PopGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    type Target = [u8];
    
    #[inline]
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::shm::{Access as SharedMemAccess};

    use std::sync::{Arc, Barrier};

    fn make_queue(item_size: usize, item_count: usize) -> Queue<SharedMemMap<SharedMem>, SharedMem> {
        let mem = SharedMem::new(Queue::required_size(item_size, item_count)).unwrap();
        let mem_map = mem.map(.., SharedMemAccess::ReadWrite).unwrap();
        unsafe { Queue::new_with_memory(item_size, item_count, mem_map, 0).unwrap() }
    }

    #[test]
    fn empty_queue_try_pop() {
        let queue = make_queue(32, 8);
        assert!(queue.try_pop().unwrap().is_none());
    }

    #[test]
    fn full_queue_try_push() {
        let queue = make_queue(32, 8);
        for _ in 0..8 {
            let _guard = queue.try_push().expect("queue unexpectedly full");
        }
        assert!(queue.try_push().is_none());
    }

    #[test]
    fn full_queue_emptied_try_pop() {
        let queue = make_queue(32, 8);
        for _ in 0..8 {
            let _guard = queue.try_push().expect("queue unexpectedly full");
        }
        for _ in 0..8 {
            let _guard = queue.try_pop().expect("queue unexpectedly empty");
        }
        assert!(queue.try_pop().unwrap().is_none());
    }

    #[test]
    fn contested_fill_and_empty() {
        let queue_size = 8;
        let queue = Arc::new(make_queue(32, queue_size));
        let barrier = Arc::new(Barrier::new(queue_size * 2));
        let mut threads = Vec::new();
        for i in 0..(queue_size * 2) {
            if i % 2 == 0 {
                threads.push(thread::spawn({
                    let queue = queue.clone();
                    let barrier = barrier.clone();
                    move || {
                        barrier.wait();
                        loop {
                            if let Some(mut guard) = queue.try_push() {
                                guard[0] = (i / 2) as u8;
                                break;
                            }
                        }
                    }
                }));
            } else {
                threads.push(thread::spawn({
                    let queue = queue.clone();
                    let barrier = barrier.clone();
                    move || {
                        barrier.wait();
                        loop {
                            if let Some(guard) = queue.try_pop().unwrap() {
                                assert!(guard[0] < queue_size as u8);
                                break;
                            }
                        }
                    }
                }));
            }
        }

        for thread in threads {
            thread.join().unwrap();
        }
    }
}