use ::{SharedMem, SharedMemMap};
use ::platform::SendableWinHandle;

use std::{io, mem, ptr, thread, usize};
use std::marker::PhantomData;
use std::borrow::Borrow;
use std::sync::atomic::{Ordering, AtomicUsize};

use platform::winapi::*;
use platform::kernel32::*;
use winhandle::*;

pub(crate) struct Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    _mem: B,
    raw_offset: usize,
    atomic: *const AtomicUsize,
    event: WinHandle,
    _phantom: PhantomData<C>,
}

pub(crate) struct MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>> + 'a,
    C: Borrow<SharedMem> + 'a,
{
    mutex: &'a Mutex<B, C>,
}

#[cfg(target_pointer_width = "32")]
pub(crate) const MUTEX_SHM_SIZE: usize = 4;
#[cfg(target_pointer_width = "64")]
pub(crate) const MUTEX_SHM_SIZE: usize = 8;

// We implement the mutex as a rudimentary thin lock, but with shared memory and
// an event to handle waiting threads. The state of the mutex is always consistent
// with the value in shared memory (0 for free, non-zero for acquired). Threads attempt to 
// acquire the mutex by an atomic fetch-add. If they fail to acquire the mutex
// they enter a loop in which they wait for the event to be signaled and then attempt
// to acquire the mutex again, this time via a CAS from zero to one.
//
// The event is during normal operation left in the unsignaled state. When a thread releases
// the mutex it checks if the shared memory value is greater than 1, indicating there was
// contention for the mutex and another thread is waiting to acquire it. In that case it
// signals the event
impl<B, C> Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub unsafe fn new_with_memory(memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");
        let atomic = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;
        (*atomic).store(0, Ordering::SeqCst);
        let event = winapi_handle_call!(CreateEventW(
            ptr::null_mut(),
            TRUE,
            FALSE,
            ptr::null(),
        ))?;

        let raw_offset = memory.borrow().offset() + offset;

        Ok(Mutex {
            _mem: memory,
            raw_offset,
            atomic,
            event,
            _phantom: PhantomData,
        })
    }

    pub unsafe fn from_handle(handle: MutexHandle, memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");
        let atomic = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;
        let raw_offset = memory.borrow().offset() + offset;

        Ok(Mutex {
            _mem: memory,
            raw_offset,
            atomic,
            event: handle.event.0,
            _phantom: PhantomData,
        })
    }

    pub fn lock(&self) -> MutexGuard<B, C> {
        // TODO: can probably relax SeqCst here, but shouldn't try to until we can test
        // on an architecture with weak memory model (e.g. ARM)
        let shared_atomic = self.shared_atomic();
        'outer: loop {
            match shared_atomic.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {}, // Value was zero, we acquired the lock
                Err(old_value) => {
                    if old_value == 1 {
                        // Inform other threads that lock was contested
                        match shared_atomic.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst) {
                            Ok(_) => {},
                            Err(2) => {},
                            Err(0) => {
                                // Lock went free, try to re-acquire from the top
                                continue 'outer;
                            },
                            _ => panic!("mutex shared memory in invalid state"),
                        }
                    } else if old_value == 2 {
                        // Value is already okay, just continue
                    } else {
                        panic!("mutex shared memory in invalid state");
                    }

                    'event: loop {
                        match unsafe { WaitForSingleObject(
                            self.event.get(),
                            INFINITE,
                        ) } {
                            WAIT_OBJECT_0 => {},
                            _ => {
                                panic!("WaitForSingleObject failed: {}", io::Error::last_os_error());
                            },
                        }

                        match shared_atomic.compare_exchange_weak(0, 2, Ordering::SeqCst, Ordering::SeqCst) {
                            Ok(_) => {
                                // We acquired the lock, now reset the event so other contending threads/future threads
                                // block on it
                                unsafe { winapi_bool_call!(assert: ResetEvent(self.event.get())); }
                                break 'event;
                            }
                            Err(_) => continue 'event,
                        }
                    }
                },
            }

            return MutexGuard { mutex: self };
        }
    }

    pub fn handle(&self) -> io::Result<MutexHandle> {
        let event = self.event.clone()?;
        Ok(MutexHandle {
            event: SendableWinHandle(event),
            raw_offset: self.raw_offset,
        })
    }

    #[inline]
    fn shared_atomic(&self) -> &AtomicUsize {
        unsafe { &*self.atomic }
    }
}

impl<'a, B, C> Drop for MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        let shared_atomic = self.mutex.shared_atomic();
        // If the value is two, there was contention and we must signal the associated event
        match shared_atomic.swap(0, Ordering::SeqCst)  {
            1 => {}, // No contention
            2 => {
                // There was contention, signal event
                if unsafe { SetEvent(self.mutex.event.get()) } != TRUE {
                    if !thread::panicking() {
                        panic!("SetEventfailed: {}", io::Error::last_os_error());
                    } else {
                        error!("SetEvent failed: {}", io::Error::last_os_error());
                    }
                }
            }
            _ => if !thread::panicking() {
                panic!("mutex shared memory in invalid state");
            } else {
                error!("mutex shared memory in invalid state");
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MutexHandle {
    event: SendableWinHandle,
    raw_offset: usize,
}

impl MutexHandle {
    pub fn raw_offset(&self) -> usize { self.raw_offset }
}