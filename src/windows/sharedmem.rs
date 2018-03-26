use ::shm::{Access as SharedMemAccess, RangeArgument};
use platform::SendableWinHandle;

use std::{io, mem, ptr};
use std::collections::Bound;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use winapi::shared::minwindef::{DWORD, FALSE};
use winapi::shared::basetsd::{SIZE_T};
use winapi::um::winnt::{PAGE_READWRITE};
use winapi::um::handleapi::{DuplicateHandle};
use winapi::um::processthreadsapi::{GetCurrentProcess};
use winapi::um::memoryapi::{FILE_MAP_READ, FILE_MAP_WRITE, FILE_MAP_ALL_ACCESS, MapViewOfFile, UnmapViewOfFile, CreateFileMappingW};
use winapi::um::sysinfoapi::{SYSTEM_INFO, GetSystemInfo};
use winhandle::*;

#[derive(Debug)]
pub(crate) struct SharedMem {
    handle: WinHandle,
    size: usize,
}

#[derive(Debug)]
pub(crate) struct SharedMemMap {
    mapping_pointer: *mut u8,
    mapping_len: usize,
    user_pointer: *mut u8,
    user_len: usize,
    access: SharedMemAccess,
    user_offset: usize,
}

unsafe impl Send for SharedMemMap {}
unsafe impl Sync for SharedMemMap {}

impl Drop for SharedMemMap {
    fn drop(&mut self) {
        unsafe {
            if UnmapViewOfFile(self.mapping_pointer as _) == FALSE {
                error!("UnmapViewOfFile failed: {}", io::Error::last_os_error());
            }
        }
    }
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
        unsafe {
            let handle = winapi_handle_call! { log: CreateFileMappingW(
                ptr::null_mut(),
                ptr::null_mut(),
                PAGE_READWRITE,
                (size >> 32) as DWORD,
                (size & 0xFFFFFFFF) as DWORD,
                ptr::null()
            )}?;

            Ok(SharedMem { handle, size })
        }
    }

    pub fn size(&self) -> usize { self.size }

    pub fn clone(&self, access: SharedMemAccess) -> io::Result<SharedMem> {
        let access = match access {
            SharedMemAccess::Read => FILE_MAP_READ,
            SharedMemAccess::ReadWrite => FILE_MAP_ALL_ACCESS,
        };

        unsafe {
            let mut handle = WinHandleTarget::new();
            winapi_bool_call!(DuplicateHandle(
                GetCurrentProcess(), self.handle.get(),
                GetCurrentProcess(), &mut *handle,
                access, FALSE, 0
            ))?;

            Ok(SharedMem { handle: handle.unwrap(), size: self.size })
        }
    }

    pub fn map<R: RangeArgument<usize>>(&self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap> {
        let raw_access = match access {
            SharedMemAccess::Read => FILE_MAP_READ,
            SharedMemAccess::ReadWrite => FILE_MAP_WRITE,
        };

        let offset = match range.start() {
            Bound::Included(i) => Some(*i),
            Bound::Excluded(i) => i.checked_add(1),
            Bound::Unbounded => Some(0),
        };
        let len = offset.and_then(|offset| match range.end() {
            Bound::Included(i) => i.checked_add(1).and_then(|x| x.checked_sub(offset)),
            Bound::Excluded(i) => i.checked_sub(offset),
            Bound::Unbounded => self.size().checked_sub(offset),
        });

        let (offset, len) = match (offset, len) {
            (Some(offset), Some(len)) if offset + len <= self.size => (offset, len),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "requested memory map range is out of bounds")),
        };

        // The offset must be a multiple of the page size
        let mapping_offset = match offset % *PAGE_SIZE {
            0 => offset,
            m => offset - m,
        };
        let mapping_len = len + offset - mapping_offset;

        unsafe {
            let addr = MapViewOfFile(
                self.handle.get(),
                raw_access,
                (mapping_offset >> 32) as DWORD,
                (mapping_offset & 0xFFFFFFFF) as DWORD,
                mapping_len as SIZE_T
            );
            if addr.is_null() {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMemMap {
                mapping_pointer: addr as _,
                mapping_len,
                user_pointer: (addr as *mut u8).offset((offset - mapping_offset) as isize),
                user_len: len,
                access,
                user_offset: offset,
            })
        }
    }
}

impl SharedMemMap {
    pub unsafe fn pointer(&self) -> *mut u8 { self.user_pointer }
    pub fn len(&self) -> usize { self.user_len }
    pub fn access(&self) -> SharedMemAccess { self.access }
    pub fn offset(&self) -> usize { self.user_offset }
}

impl Serialize for SharedMem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        (SendableWinHandle(&self.handle), self.size).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SharedMem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        let (handle, size): (SendableWinHandle, usize) = Deserialize::deserialize(deserializer)?;
        Ok(SharedMem { handle: handle.0, size })
    }
}

lazy_static! {
    static ref PAGE_SIZE: usize = unsafe {
        let mut system_info: SYSTEM_INFO = mem::zeroed();
        GetSystemInfo(&mut system_info);
        system_info.dwPageSize as usize
    };
}