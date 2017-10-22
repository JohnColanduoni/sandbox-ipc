use platform;

use std::{io};
use std::collections::range::RangeArgument;
use std::borrow::Borrow;

use uuid::Uuid;

pub mod queue;

#[derive(Serialize, Deserialize, Debug)]
pub struct SharedMem {
    pub(crate) inner: platform::SharedMem,
    pub(crate) token: Uuid,
}

pub struct SharedMemMap<T = SharedMem> where
    T: Borrow<SharedMem>
{
    inner: platform::SharedMemMap<T>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SharedMemAccess {
    Read,
    ReadWrite,
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
        let inner = platform::SharedMem::new(size)?;
        let token = Uuid::new_v4();
        Ok(SharedMem { inner, token })
    }

    pub fn size(&self) -> usize { self.inner.size() }

    pub fn clone(&self, access: SharedMemAccess) -> io::Result<SharedMem> {
        let inner = self.inner.clone(access)?;
        Ok(SharedMem { inner, token: self.token })
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
        let inner = platform::SharedMem::map_with(t, range, access)?;
        Ok(SharedMemMap { inner })
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    pub fn unmap(self) -> io::Result<T> {
        self.inner.unmap()
    }

    pub unsafe fn pointer(&self) -> *mut u8 { self.inner.pointer() }
    pub fn len(&self) -> usize { self.inner.len() }
    pub fn access(&self) -> SharedMemAccess { self.inner.access() }
    pub fn offset(&self) -> usize { self.inner.offset() }

    pub(crate) fn token(&self) -> Uuid { self.inner.object().token }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::{MessageChannel, check_send};

    use tokio;
    use futures::{Sink, Stream};

    #[test]
    fn shared_mem_map_is_send() {
        let memory = SharedMem::new(4096).unwrap();
        let memory = memory.map(.., SharedMemAccess::ReadWrite).unwrap();
        check_send(&memory);
    }

    #[test]
    fn shared_mem_is_send() {
        let memory = SharedMem::new(4096).unwrap();
        check_send(&memory);
    }

    #[test]
    fn send_mem_same_process() {
        let mut reactor = tokio::reactor::Core::new().unwrap();
        let (a, b) = MessageChannel::pair(&reactor.handle(), 8192).unwrap();

        let test_bytes: &[u8] = b"hello";

        let memory = SharedMem::new(0x1000).unwrap();
        unsafe {
            let mapping = memory.map_ref(.., SharedMemAccess::ReadWrite).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            slice[0..test_bytes.len()].copy_from_slice(test_bytes);
        }

        let _a = reactor.run(a.send(memory)).unwrap();
        let (message, _b) = reactor.run(b.into_future()).map_err(|(err, _)| err).unwrap();
        let memory: SharedMem = message.unwrap();

        unsafe {
            let mapping = memory.map_ref(.., SharedMemAccess::Read).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            assert_eq!(&slice[0..test_bytes.len()], test_bytes);
        }
    }

    #[test]
    fn big_shm() {
        let memory = SharedMem::new(64 * 1024 * 1024).unwrap();
        let _mapping = memory.map_ref(.., SharedMemAccess::ReadWrite).unwrap();
    }
}
