use ::SharedMemAccess;

use std::{io};
use std::collections::Bound;
use std::collections::range::RangeArgument;
use std::borrow::Borrow;

#[derive(Serialize, Deserialize, Debug)]
pub struct SharedMem {
    size: usize,
}

pub struct SharedMemMap<T = SharedMem> where
    T: Borrow<SharedMem>
{
    mem: T,
    pointer: *mut u8,
    len: usize,
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
       unimplemented!()
    }

    pub fn size(&self) -> usize { self.size }

    pub fn clone(&self, read_only: bool) -> io::Result<SharedMem> {
        unimplemented!()
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
        unimplemented!()
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    pub fn unmap(self) -> io::Result<T> {
        unimplemented!()
    }

    pub unsafe fn pointer(&self) -> *mut u8 { self.pointer }
    pub fn len(&self) -> usize { self.len }
}