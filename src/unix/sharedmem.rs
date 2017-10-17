use ::SharedMemAccess;
use platform::{ScopedFd, SendableFd};

use std::{io, mem, ptr};
use std::collections::Bound;
use std::collections::range::RangeArgument;
use std::borrow::Borrow;
use std::ffi::CString;

use uuid::Uuid;
use platform::libc;

#[derive(Serialize, Deserialize, Debug)]
pub struct SharedMem {
    fd: ScopedFd,
    size: usize,
}

pub struct SharedMemMap<T = SharedMem> where
    T: Borrow<SharedMem>
{
    mem: T,
    pointer: *mut u8,
    len: usize,
}

impl<T> Drop for SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        unsafe {
            if libc::munmap(self.pointer as _, self.len) < 0 {
                error!("munmap failed: {}", io::Error::last_os_error());
            }
        }
    }
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
        unsafe {
            let mut name = format!("/{}", Uuid::new_v4().simple());
            name.truncate(30); // macOS doesn't like names longer than this for shared memory
            let name = CString::new(name).unwrap();
            // TODO: linux has a better way of doing this I think
            let fd = libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o700);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::shm_unlink(name.as_ptr()) < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::ftruncate(fd, size as _) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMem {
                fd: ScopedFd(fd),
                size,
            })
        }
    }

    pub fn size(&self) -> usize { self.size }

    pub fn clone(&self, read_only: bool) -> io::Result<SharedMem> {
        let fd = unsafe { libc::dup(self.fd.0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SharedMem {
            fd: ScopedFd(fd),
            size: self.size,
        })
    }

    pub fn map<R>(self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<Self>> where
        R: RangeArgument<usize>,
    {
        Self::map_with(self, range, access)
    }

    pub fn map_ref<R>(&self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<&Self>> where
        R: RangeArgument<usize>,
    {
        Self::map_with(self, range, access)
    }

    pub fn map_with<T, R>(t: T, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<T>> where
        T: Borrow<SharedMem>,
        R: RangeArgument<usize>,
    {
        let prot = match access {
            SharedMemAccess::Read => libc::PROT_READ,
            SharedMemAccess::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
        };

        let offset = match range.start() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => i + 1,
            Bound::Unbounded => 0,
        };
        let len = match range.start() {
            Bound::Included(i) => i + 1,
            Bound::Excluded(i) => *i,
            Bound::Unbounded => t.borrow().size(),
        };

        let ptr = unsafe { libc::mmap(ptr::null_mut(), len as _, prot, libc::MAP_SHARED, t.borrow().fd.0, offset as _) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(SharedMemMap {
            mem: t,
            pointer: ptr as *mut u8,
            len: len,
        })
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    pub fn unmap(self) -> io::Result<T> {
        unsafe {
            if libc::munmap(self.pointer as _, self.len) < 0 {
                return Err(io::Error::last_os_error());
            }

            // We have to do these gymnastics to prevent the destructor from running
            let memory = ptr::read(&self.mem);
            mem::forget(self);

            Ok(memory)
        }
    }

    pub unsafe fn pointer(&self) -> *mut u8 { self.pointer }
    pub fn len(&self) -> usize { self.len }
}