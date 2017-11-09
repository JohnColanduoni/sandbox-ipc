use ::{USIZE_SIZE};
use ::shm::{SharedMemMap};

use std::{io, mem, thread, usize};
use std::borrow::Borrow;
use std::sync::atomic::{Ordering, AtomicUsize};

use platform::libc;

pub(crate) struct Mutex {
    mem: SharedMemMap,
    raw_offset: usize,
    refcount: *const AtomicUsize,
    pthread_mutex: *mut libc::pthread_mutex_t,
}

impl Drop for Mutex {
    fn drop(&mut self) {
        if unsafe { (*self.refcount).fetch_sub(1, Ordering::SeqCst) } == 1 {
            unsafe { libc::pthread_mutex_destroy(self.pthread_mutex); }
        }
    }
}

unsafe impl Send for Mutex {}

unsafe impl Sync for Mutex {}

pub(crate) struct MutexGuard<'a> {
    mutex: &'a Mutex,
}

pub(crate) const MUTEX_SHM_SIZE: usize = USIZE_SIZE + PTHREAD_MUTEX_SIZE_BOUND;

// We use a bound on the pthread mutex size because its size is platform specific
// TODO: remove once const fn is stable
const PTHREAD_MUTEX_SIZE_BOUND: usize = 128 - USIZE_SIZE;

impl Mutex {
    pub unsafe fn new_with_memory(memory: SharedMemMap, offset: usize) -> io::Result<Self> {
        assert!(memory.len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");

        assert!(mem::size_of::<libc::pthread_mutex_t>() <= PTHREAD_MUTEX_SIZE_BOUND, "pthread_mutex_t too large on this platform");

        let raw_offset = memory.offset() + offset;

        let refcount = memory.pointer().offset(offset as isize) as *const AtomicUsize;
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
            mem: memory,
            raw_offset,
            refcount,
            pthread_mutex,
        })
    }

    pub unsafe fn from_handle(handle: MutexHandle, memory: SharedMemMap, offset: usize) -> io::Result<Self> {
        assert!(memory.len() >= offset + MUTEX_SHM_SIZE, "insufficient space for mutex");
        assert!((memory.pointer() as usize + offset) % mem::size_of::<usize>() == 0, "shared memory for IPC mutex must be aligned");

        assert!(mem::size_of::<libc::pthread_mutex_t>() <= PTHREAD_MUTEX_SIZE_BOUND, "pthread_mutex_t too large on this platform");

        let refcount = memory.pointer().offset(offset as isize) as *const AtomicUsize;
        (*refcount).fetch_add(1, Ordering::SeqCst);
        let pthread_mutex = refcount.offset(1) as *mut libc::pthread_mutex_t;

        Ok(Mutex {
            mem: memory,
            raw_offset: handle.raw_offset,
            refcount,
            pthread_mutex,
        })
    }

    pub fn lock(&self) -> MutexGuard {
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

    pub fn memory(&self) -> &SharedMemMap {
        self.mem.borrow()
    }
}

impl<'a> Drop for MutexGuard<'a> {
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