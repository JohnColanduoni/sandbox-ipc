use ::shm::SharedMemAccess;
use platform::{ScopedFd};

use std::{io, mem, ptr};
use std::collections::Bound;
use std::collections::range::RangeArgument;
use std::borrow::Borrow;
use std::ffi::CString;

use uuid::Uuid;
use platform::libc;

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SharedMem {
    rw_fd: Option<ScopedFd>,
    ro_fd: ScopedFd,
    size: usize,
}

pub(crate) struct SharedMemMap<T = SharedMem> where
    T: Borrow<::shm::SharedMem>
{
    mem: T,
    pointer: *mut u8,
    len: usize,
    pointer_offset: usize,
    access: SharedMemAccess,
}

impl<T> Drop for SharedMemMap<T> where
    T: Borrow<::shm::SharedMem>,
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
            let ro_fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0o700);
            if ro_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::shm_unlink(name.as_ptr()) < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::ftruncate(fd, size as _) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMem {
                rw_fd: Some(ScopedFd(fd)),
                ro_fd: ScopedFd(ro_fd),
                size,
            })
        }
    }

    pub fn size(&self) -> usize { self.size }

    pub fn clone(&self, access: SharedMemAccess) -> io::Result<SharedMem> {
        match access {
            SharedMemAccess::Read => {
                let ro_fd = unsafe { libc::dup(self.ro_fd.0) };
                if ro_fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(SharedMem {
                    rw_fd: None,
                    ro_fd: ScopedFd(ro_fd),
                    size: self.size,
                })
            },
            SharedMemAccess::ReadWrite => {
                if let Some(rw_fd) = self.rw_fd.as_ref() {
                    let rw_fd = unsafe { libc::dup(rw_fd.0) };
                    if rw_fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let ro_fd = unsafe { libc::dup(self.ro_fd.0) };
                    if ro_fd < 0 {
                        return Err(io::Error::last_os_error());
                    }

                    Ok(SharedMem {
                        rw_fd: Some(ScopedFd(rw_fd)),
                        ro_fd: ScopedFd(ro_fd),
                        size: self.size,
                    })
                } else {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "this shared memory handle is read-only"));
                }
            },
        }
    }

    pub fn map_with<T, R>(t: T, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<T>> where
        T: Borrow<::shm::SharedMem>,
        R: RangeArgument<usize>,
    {
        let (prot, fd) = match access {
            SharedMemAccess::Read => (libc::PROT_READ, t.borrow().inner.ro_fd.0),
            SharedMemAccess::ReadWrite => {
                if let Some(rw_fd) = t.borrow().inner.rw_fd.as_ref() {
                    (libc::PROT_READ | libc::PROT_WRITE, rw_fd.0)
                } else {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "this shared memory handle is read-only"));
                }
            },
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

        let ptr = unsafe { libc::mmap(ptr::null_mut(), len as _, prot, libc::MAP_SHARED, fd, offset as _) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(SharedMemMap {
            mem: t,
            pointer: ptr as *mut u8,
            len: len,
            pointer_offset: offset,
            access,
        })
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<::shm::SharedMem>,
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
    pub fn access(&self) -> SharedMemAccess { self.access }
    pub fn offset(&self) -> usize { self.pointer_offset }

    pub fn object(&self) -> &::shm::SharedMem {
        self.mem.borrow()
    }
}
