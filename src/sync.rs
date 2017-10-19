use ::{SharedMem, SharedMemMap, SharedMemAccess};
use platform;

use std::{io, mem, thread};
use std::borrow::Borrow;
use std::sync::{LockResult, PoisonError};
use std::sync::atomic::{Ordering, AtomicBool};

pub struct Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub(crate) inner: platform::Mutex<B, C>,
    poison: *const AtomicBool,
}

pub struct MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>> + 'a,
    C: Borrow<SharedMem> + 'a,
{
    mutex: &'a Mutex<B, C>,
    _inner: platform::MutexGuard<'a, B, C>,
}

#[cfg(target_pointer_width = "32")]
pub const MUTEX_SHM_SIZE: usize = platform::MUTEX_SHM_SIZE + 4;
#[cfg(target_pointer_width = "64")]
pub const MUTEX_SHM_SIZE: usize = platform::MUTEX_SHM_SIZE + 8;

// On top of the OS-level mutex, we add a usize (protected by the mutex) that signals
// that the lock is poisoned (possibly by a thread in another process).
impl<B, C> Mutex<B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    pub unsafe fn new_with_memory(memory: B, offset: usize) -> io::Result<Self> {
        Self::with_inner(memory, offset, |memory| platform::Mutex::new_with_memory(memory, offset))
    }

    pub fn from_handle(handle: MutexHandle, memory: B) -> io::Result<Self> {
        let local_offset = handle.inner.raw_offset().checked_sub(memory.borrow().offset())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mapping does not contain memory of shared mutex"))?;
        if memory.borrow().len() < local_offset + MUTEX_SHM_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "mapping does not contain memory of shared mutex"));
        }
        unsafe { Self::with_inner(memory, local_offset, move |memory| platform::Mutex::from_handle(handle.inner, memory, local_offset)) }
    }

    pub(crate) unsafe fn with_inner<F>(memory: B, offset: usize, f: F) -> io::Result<Self> where
        F: FnOnce(B) -> io::Result<platform::Mutex<B, C>>,
    {
        if memory.borrow().len() < offset + MUTEX_SHM_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "out of range offset for mutex shared memory"));
        }
        if (memory.borrow().pointer() as usize + offset) % mem::size_of::<usize>() != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "memory for mutex must be aligned"));
        }
        if memory.borrow().access() != SharedMemAccess::ReadWrite {
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

    pub fn lock(&self) -> LockResult<MutexGuard<B, C>> {
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

    pub fn handle(&self) -> io::Result<MutexHandle> {
        let inner = self.inner.handle()?;
        Ok(MutexHandle { inner })
    }

    fn poison_flag(&self) -> &AtomicBool {
        unsafe { &*self.poison }
    }
}

impl<'a, B, C> Drop for MutexGuard<'a, B, C> where
    B: Borrow<SharedMemMap<C>>,
    C: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        if thread::panicking() {
            // Indicate we are panicking, both to this instance and possible other instances in different processes
            self.mutex.poison_flag().store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MutexHandle {
    inner: platform::MutexHandle,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::{SharedMemAccess};

    use std::{mem, thread};
    use std::time::Duration;
    use std::sync::{Barrier, Arc};

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

        let memory = Arc::new(SharedMem::new(MUTEX_SHM_SIZE).unwrap());
        let memory_map = SharedMem::map_with(memory.clone(), .., SharedMemAccess::ReadWrite).unwrap();
        let mutex = unsafe { Mutex::new_with_memory(memory_map, 0).unwrap() };

        let guard = mutex.lock().unwrap();
        let thread = thread::spawn({
            let barrier = barrier.clone();
            let handle = mutex.handle().unwrap();
            let value = value.clone();
            move || {
                let memory_map = SharedMem::map_with(memory.clone(), .., SharedMemAccess::ReadWrite).unwrap();
                let mutex = Mutex::from_handle(handle, memory_map).unwrap();
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
