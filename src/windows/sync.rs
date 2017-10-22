use ::{CACHE_LINE};
use ::shm::{SharedMem, SharedMemMap};
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
    mem: B,
    raw_offset: usize,
    atomic: *const AtomicUsize,
    semaphore: WinHandle,
    _phantom: PhantomData<C>,
}

pub(crate) struct MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>> + 'a,
    C: Borrow<SharedMem> + 'a,
{
    mutex: &'a Mutex<B, C>,
}

pub(crate) const MUTEX_SHM_SIZE: usize = CACHE_LINE;

// We implement the mutex as a rudimentary thin lock, but with shared memory and
// a semaphore to handle waiting threads. The state of the mutex is always consistent
// with the value in shared memory (0 for free, non-zero for acquired). Threads attempt to 
// acquire the mutex by an atomic fetch-add. If they fail to acquire the mutex they wait
// on the semaphore. When releasing the mutex via a fetch-sub operation, the releasing thread
// checks if the count before the operation was greater than one, and if so performs a release
// operation on the semaphore.
impl<B, C> Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub unsafe fn new_with_memory(memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");
        let atomic = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;
        (*atomic).store(0, Ordering::SeqCst);
        let semaphore = winapi_handle_call!(CreateSemaphoreW(
            ptr::null_mut(),
            0,
            1,
            ptr::null(),
        ))?;

        let raw_offset = memory.borrow().offset() + offset;

        Ok(Mutex {
            mem: memory,
            raw_offset,
            atomic,
            semaphore,
            _phantom: PhantomData,
        })
    }

    pub unsafe fn from_handle(handle: MutexHandle, memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");
        let atomic = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;

        Ok(Mutex {
            mem: memory,
            raw_offset: handle.raw_offset,
            atomic,
            semaphore: handle.semaphore.0,
            _phantom: PhantomData,
        })
    }

    pub fn lock(&self) -> MutexGuard<B, C> {
        // TODO: can probably relax SeqCst here, but shouldn't try to until we can test
        // on an architecture with weak memory model (e.g. ARM)
        let shared_atomic = self.shared_atomic();
        'outer: loop {
            match shared_atomic.fetch_add(1, Ordering::SeqCst) {
                // Mutex was free
                0 => break,
                // Mutex is contested
                _ => {
                     match unsafe { WaitForSingleObject(
                        self.semaphore.get(),
                        INFINITE,
                    ) } {
                        WAIT_OBJECT_0 => {},
                        _ => {
                            panic!("WaitForSingleObject failed: {}", io::Error::last_os_error());
                        },
                    }
                    break;
                },
            }
        }

        return MutexGuard { mutex: self };
    }

    pub fn handle(&self) -> io::Result<MutexHandle> {
        Ok(MutexHandle {
            semaphore: SendableWinHandle(self.semaphore.clone()?),
            raw_offset: self.raw_offset,
        })
    }

    #[inline]
    fn shared_atomic(&self) -> &AtomicUsize {
        unsafe { &*self.atomic }
    }

    pub(crate) fn memory(&self) -> &::shm::SharedMemMap<C> {
        self.mem.borrow()
    }
}

impl<'a, B, C> Drop for MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        let shared_atomic = self.mutex.shared_atomic();
        match shared_atomic.fetch_sub(1, Ordering::SeqCst)  {
            0 => if !thread::panicking() {
                panic!("mutex shared memory in invalid state");
            } else {
                error!("mutex shared memory in invalid state");
            },
            1 => {}, // No contention
            _ => {
                // There was contention, release semaphore
                if unsafe { ReleaseSemaphore(self.mutex.semaphore.get(), 1, ptr::null_mut()) } != TRUE {
                    if !thread::panicking() {
                        panic!("ReleaseSemaphore failed: {}", io::Error::last_os_error());
                    } else {
                        error!("ReleaseSemaphore failed: {}", io::Error::last_os_error());
                    }
                }
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MutexHandle {
    semaphore: SendableWinHandle,
    raw_offset: usize,
}

impl MutexHandle {
    pub fn raw_offset(&self) -> usize { self.raw_offset }
}