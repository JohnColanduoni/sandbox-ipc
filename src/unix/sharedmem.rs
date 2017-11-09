use ::shm::{Access as SharedMemAccess, RangeArgument};
use platform::{ScopedFd};

use std::{io, ptr};
use std::collections::Bound;
use std::ffi::CString;
use std::os::raw::c_int;

use uuid::Uuid;
use platform::libc;

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct SharedMem {
    rw_fd: Option<ScopedFd>,
    ro_fd: ScopedFd,
    size: usize,
}

unsafe impl Send for SharedMem {
}
unsafe impl Sync for SharedMem {
}

#[derive(Debug)]
pub(crate) struct SharedMemMap {
    mapping_pointer: *mut u8,
    user_pointer: *mut u8,
    mapping_len: usize,
    user_len: usize,
    mapping_offset: usize,
    user_offset: usize,
    access: SharedMemAccess,
}

unsafe impl Send for SharedMemMap {}
unsafe impl Sync for SharedMemMap {}

impl Drop for SharedMemMap {
    fn drop(&mut self) {
        unsafe {
            if libc::munmap(self.mapping_pointer as _, self.mapping_len) < 0 {
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
            let fd = ScopedFd(fd);
            let ro_fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0o700);
            if ro_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let ro_fd = ScopedFd(ro_fd);
            if libc::shm_unlink(name.as_ptr()) < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::ftruncate(fd.0, size as _) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMem {
                rw_fd: Some(fd),
                ro_fd,
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

    pub fn map<R>(&self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap> where
        R: RangeArgument<usize>,
    {
        let (prot, fd) = match access {
            SharedMemAccess::Read => (libc::PROT_READ, self.ro_fd.0),
            SharedMemAccess::ReadWrite => {
                if let Some(rw_fd) = self.rw_fd.as_ref() {
                    (libc::PROT_READ | libc::PROT_WRITE, rw_fd.0)
                } else {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "this shared memory handle is read-only"));
                }
            },
        };

        let offset = match range.start() {
            Bound::Included(i) => Some(*i),
            Bound::Excluded(i) => i.checked_add(1),
            Bound::Unbounded => Some(0),
        };
        let end = match range.start() {
            Bound::Included(i) => i.checked_add(1),
            Bound::Excluded(i) => Some(*i),
            Bound::Unbounded => Some(self.size),
        };

        let (offset, end) = match (offset, end) {
            (Some(offset), Some(end)) if offset <= end && end <= self.size => (offset, end),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "shared memory map range is out of bounds")),
        };

        let page_size = *PAGE_SIZE;
        let user_offset_shift = offset % page_size;
        let user_end_shift = match end % page_size {
            0 => 0,
            x => page_size - x,
        };
        let mapping_offset = offset - user_offset_shift;
        let mapping_len = end + user_end_shift - mapping_offset;

        let mapping_ptr = unsafe { libc::mmap(ptr::null_mut(), mapping_len as _, prot, libc::MAP_SHARED, fd, mapping_offset as _) };
        if mapping_ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(SharedMemMap {
            mapping_pointer: mapping_ptr as *mut u8,
            user_pointer: unsafe { (mapping_ptr as *mut u8).offset(user_offset_shift as isize) },
            mapping_len,
            user_len: end - offset,
            mapping_offset,
            user_offset: offset,
            access,
        })
    }
}

impl SharedMemMap {
    pub unsafe fn pointer(&self) -> *mut u8 { self.user_pointer }
    pub fn len(&self) -> usize { self.user_len }
    pub fn access(&self) -> SharedMemAccess { self.access }
    pub fn offset(&self) -> usize { self.user_offset }
}

lazy_static! {
    static ref PAGE_SIZE: usize = unsafe {
        getpagesize() as usize
    };
}

#[link = "c"]
extern "C" {
    fn getpagesize() -> c_int;
}
