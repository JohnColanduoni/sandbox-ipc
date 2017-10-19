use ::SharedMemAccess;
use platform::SendableWinHandle;

use std::{io, mem, ptr};
use std::collections::Bound;
use std::collections::range::RangeArgument;
use std::borrow::Borrow;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use platform::winapi::*;
use platform::kernel32::*;
use winhandle::*;

#[derive(Debug)]
pub struct SharedMem {
    handle: WinHandle,
    size: usize,
}

pub struct SharedMemMap<T = SharedMem> where
    T: Borrow<SharedMem>
{
    mem: T,
    pointer: *mut u8,
    len: usize,
    access: SharedMemAccess,
    pointer_offset: usize,
}

impl<T> Drop for SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        unsafe {
            if UnmapViewOfFile(self.pointer as _) == FALSE {
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

    pub fn clone(&self, read_only: bool) -> io::Result<SharedMem> {
        let access = if read_only { FILE_MAP_READ } else { FILE_MAP_ALL_ACCESS };

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

    pub fn map_with<T, R>(t: T, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<T>> where
        T: Borrow<SharedMem>,
        R: RangeArgument<usize>,
    {
        let raw_access = match access {
            SharedMemAccess::Read => FILE_MAP_READ,
            SharedMemAccess::ReadWrite => FILE_MAP_WRITE,
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

        unsafe {
            let addr = MapViewOfFile(
                t.borrow().handle.get(),
                raw_access,
                (offset >> 32) as DWORD,
                (offset & 0xFFFFFFFF) as DWORD,
                len as SIZE_T
            );
            if addr.is_null() {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMemMap {
                mem: t,
                pointer: addr as _,
                len,
                access,
                pointer_offset: offset,
            })
        }
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    pub fn unmap(self) -> io::Result<T> {
        unsafe {
            let pointer = self.pointer;
            // We have to do these gymnastics to prevent the destructor from running
            let memory = ptr::read(&self.mem);
            mem::forget(self);

            winapi_bool_call!(UnmapViewOfFile(
                pointer as _
            ))?;

            Ok(memory)
        }
    }

    pub unsafe fn pointer(&self) -> *mut u8 { self.pointer }
    pub fn len(&self) -> usize { self.len }
    pub fn access(&self) -> SharedMemAccess { self.access }
    pub fn offset(&self) -> usize { self.pointer_offset }
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
