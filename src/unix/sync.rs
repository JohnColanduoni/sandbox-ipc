use ::shm::{SharedMem, SharedMemMap};

use std::{io, mem, thread, usize};
use std::marker::PhantomData;
use std::borrow::Borrow;
use std::sync::atomic::{Ordering, AtomicUsize};

use platform::libc;

pub(crate) struct Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    _mem: B,
    raw_offset: usize,
    refcount: *const AtomicUsize,
    pthread_mutex: *mut libc::pthread_mutex_t,
    _phantom: PhantomData<C>,
}

impl<B, C> Drop for Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        if unsafe { (*self.refcount).fetch_sub(1, Ordering::SeqCst) } == 1 {
            unsafe { libc::pthread_mutex_destroy(self.pthread_mutex); }
        }
    }
}

pub(crate) struct MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>> + 'a,
    C: Borrow<SharedMem> + 'a,
{
    mutex: &'a Mutex<B, C>,
}

pub(crate) const MUTEX_SHM_SIZE: usize = mem::size_of::<usize>() + mem::size_of::<libc::pthread_mutex_t>();

impl<B, C> Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub unsafe fn new_with_memory(memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");
        let raw_offset = memory.borrow().offset() + offset;

        let refcount = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;
        (*refcount).store(1, Ordering::SeqCst);

        let pthread_mutex = refcount.offset(1) as *mut libc::pthread_mutex_t;
        let mut attr: libc::pthread_mutexattr_t = mem::zeroed();
        match libc::pthread_mutexattr_init(&mut attr) {
            0 => {},
            err => return Err(io::Error::from_raw_os_error(err)),
        }
        match libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED) {
            0 => {},
            err => return Err(io::Error::from_raw_os_error(err)),
        }
        match libc::pthread_mutex_init(pthread_mutex, &attr) {
            0 => {},
            err => return Err(io::Error::from_raw_os_error(err)),
        }
        libc::pthread_mutexattr_destroy(&mut attr);

        Ok(Mutex {
            _mem: memory,
            raw_offset,
            refcount,
            pthread_mutex,
            _phantom: PhantomData,
        })
    }

    pub unsafe fn from_handle(handle: MutexHandle, memory: B, offset: usize) -> io::Result<Self> {
        assert!(memory.borrow().len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");

        let refcount = memory.borrow().pointer().offset(offset as isize) as *const AtomicUsize;
        (*refcount).fetch_add(1, Ordering::SeqCst);
        let pthread_mutex = refcount.offset(1) as *mut libc::pthread_mutex_t;

        Ok(Mutex {
            _mem: memory,
            raw_offset: handle.raw_offset,
            refcount,
            pthread_mutex,
            _phantom: PhantomData,
        })
    }

    pub fn lock(&self) -> MutexGuard<B, C> {
        match unsafe { libc::pthread_mutex_lock(self.pthread_mutex) } {
            0 => MutexGuard { mutex: self },
            err => panic!("pthread_mutex_lock failed: {}", io::Error::from_raw_os_error(err)),
        }
    }

    pub fn handle(&self) -> io::Result<MutexHandle> {
        Ok(MutexHandle {
            raw_offset: self.raw_offset,
        })
    }
}

impl<'a, B, C> Drop for MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        match unsafe { libc::pthread_mutex_unlock(self.mutex.pthread_mutex) } {
            0 => {},
            err => if !thread::panicking() {
                panic!("pthread_mutex_unlock failed: {}", io::Error::from_raw_os_error(err));
            } else {
                error!("pthread_mutex_unlock failed: {}", io::Error::from_raw_os_error(err));
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct MutexHandle {
    raw_offset: usize,
}

impl MutexHandle {
    pub fn raw_offset(&self) -> usize { self.raw_offset }
}