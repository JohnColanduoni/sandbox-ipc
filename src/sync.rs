use ::shm::{SharedMemMap, Access as SharedMemAccess};
use platform;

use std::{io, mem, thread};
use std::borrow::Borrow;
use std::sync::{LockResult, PoisonError};
use std::sync::atomic::{Ordering, AtomicBool};

use uuid::Uuid;

/// An analogue of `std::sync::Mutex` which can operate within shared memory.
pub struct Mutex {
    pub(crate) inner: platform::Mutex,
    poison: *const AtomicBool,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {}

pub struct MutexGuard<'a> {
    mutex: &'a Mutex,
    _inner: platform::MutexGuard<'a>,
}

/// The amount of shared memory space required to hold a `Mutex`.
pub const MUTEX_SHM_SIZE: usize = platform::MUTEX_SHM_SIZE + mem::size_of::<usize>();

// On top of the OS-level mutex, we add a usize (protected by the mutex) that signals
// that the lock is poisoned (possibly by a thread in another process).
impl Mutex {
    /// Creates a brand new `Mutex` in the given shared memory location.
    /// 
    /// This can *only* be used to create a brand new `Mutex`. It cannot be used to create a handle to an
    /// existing `Mutex` already created at the given location in shared memory. To send the `Mutex` to another
    /// process you must send the shared memory region and a `MutexHandle` produced via the `handle()` function.
    /// 
    /// # Panics
    /// 
    /// This function will panic if there is not `MUTEX_SHM_SIZE` bytes of memory available at the
    /// given `offset`, or if the memory is not aligned to a pointer width within the shared memory section.
    pub unsafe fn new_with_memory(memory: SharedMemMap, offset: usize) -> io::Result<Self> {
        Self::with_inner(memory, offset, |memory| platform::Mutex::new_with_memory(memory, offset))
    }

    /// Establishes a new reference to a `Mutex` previously created with `new_with_memory`.
    pub unsafe fn from_handle(handle: MutexHandle, memory: SharedMemMap) -> io::Result<Self> {
        if memory.borrow().token() != handle.shm_token {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the mutex is not associated with the given shared memory"));
        }
        let local_offset = handle.inner.raw_offset().checked_sub(memory.borrow().offset())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping does not contain memory of shared mutex"))?;
        if memory.borrow().len() < local_offset + MUTEX_SHM_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "mapping does not contain memory of shared mutex"));
        }
        Self::with_inner(memory, local_offset, move |memory| platform::Mutex::from_handle(handle.inner, memory, local_offset))
    }

    pub(crate) unsafe fn with_inner<F>(memory: SharedMemMap, offset: usize, f: F) -> io::Result<Self> where
        F: FnOnce(SharedMemMap) -> io::Result<platform::Mutex>,
    {
        if memory.len() < offset + MUTEX_SHM_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "out of range offset for mutex shared memory"));
        }
        if (memory.pointer() as usize + offset) % mem::size_of::<usize>() != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "memory for mutex must be aligned"));
        }
        if memory.access() != SharedMemAccess::ReadWrite {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "memory for mutex must be read-write"));
        }
        let poison = memory.borrow().pointer().offset((offset + platform::MUTEX_SHM_SIZE) as isize) as *const AtomicBool;
        (*poison).store(false, Ordering::SeqCst);
        let inner = f(memory)?;
        Ok(Mutex {
            inner,
            poison,
        })
    }

    /// Acquires an exclusive lock on the `Mutex`, including any instances created from the same
    /// `MutexHandle`.
    pub fn lock(&self) -> LockResult<MutexGuard> {
        let guard = self.inner.lock();
        let guard = MutexGuard {
            mutex: self,
            _inner: guard,
        };
        // TODO: we can probably relax SeqCst, but shouldn't do so before testing on
        // a weak memory architecture
        if self.poison_flag().load(Ordering::SeqCst) {
            Err(PoisonError::new(guard))
        } else {
            Ok(guard)
        }
    }

    /// Creates a new handle to the `Mutex` that can be transmitted to other processes.
    pub fn handle(&self) -> io::Result<MutexHandle> {
        let inner = self.inner.handle()?;
        Ok(MutexHandle { inner, shm_token: self.inner.memory().token() })
    }

    fn poison_flag(&self) -> &AtomicBool {
        unsafe { &*self.poison }
    }
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        if thread::panicking() {
            // Indicate we are panicking, both to this instance and possible other instances in different processes
            self.mutex.poison_flag().store(true, Ordering::SeqCst);
        }
    }
}

/// A handle to a `Mutex` that exists in shared memory.
/// 
/// This can be sent over any medium capable of transmitting OS resources (e.g. `MessageChannel`). To reconstitute a working `Mutex`,
/// a reference to the `SharedMem` holding it must be transmitted as well.
#[derive(Serialize, Deserialize, Debug)]
pub struct MutexHandle {
    inner: platform::MutexHandle,
    shm_token: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::shm::{Access as SharedMemAccess, SharedMem};
    use ::check_send;

    use std::{mem, thread};
    use std::time::Duration;
    use std::sync::{Barrier, Arc};

    #[test]
    fn mutex_is_send() {
        let memory = SharedMem::new(MUTEX_SHM_SIZE).unwrap();
        let memory = memory.map(.. MUTEX_SHM_SIZE, SharedMemAccess::ReadWrite).unwrap();
        let mutex = unsafe { Mutex::new_with_memory(memory, 0).unwrap() };
        check_send(&mutex);
    }

    #[test]
    fn uncontested_mutex_lock() {
        let memory = SharedMem::new(MUTEX_SHM_SIZE).unwrap();
        let memory = memory.map(.. MUTEX_SHM_SIZE, SharedMemAccess::ReadWrite).unwrap();
        let mutex = unsafe { Mutex::new_with_memory(memory, 0).unwrap() };
        {
            let _guard = mutex.lock().unwrap();
        }
        {
            let _guard = mutex.lock().unwrap();
        }
    }

    #[test]
    fn single_process_contested_mutex_lock() {
        let barrier = Arc::new(Barrier::new(2));
        let value = Arc::new(AtomicBool::new(false));

        let memory = SharedMem::new(MUTEX_SHM_SIZE).unwrap();
        let memory_map = memory.map(.., SharedMemAccess::ReadWrite).unwrap();
        let mutex = unsafe { Mutex::new_with_memory(memory_map, 0).unwrap() };

        let guard = mutex.lock().unwrap();
        let thread = thread::spawn({
            let barrier = barrier.clone();
            let handle = mutex.handle().unwrap();
            let value = value.clone();
            move || {
                let memory_map = memory.map(.., SharedMemAccess::ReadWrite).unwrap();
                let mutex = unsafe { Mutex::from_handle(handle, memory_map).unwrap() };
                barrier.wait();
                let _guard = mutex.lock().unwrap();
                value.store(true, Ordering::SeqCst);
            }
        });
        barrier.wait();
        thread::sleep(Duration::from_millis(100));
        assert!(!value.load(Ordering::SeqCst));
        mem::drop(guard);
        thread.join().unwrap();
        assert!(value.load(Ordering::SeqCst));
    }
}
