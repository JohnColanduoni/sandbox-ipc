use platform;

use std::{io};
use std::ops::{Range, RangeFull, RangeTo, RangeFrom};
use std::collections::Bound;
use std::sync::Arc;

use uuid::Uuid;

pub mod queue;

pub use self::queue::Queue;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SharedMem(Arc<_SharedMem>);

#[derive(Debug)]
#[derive(Serialize, Deserialize)]
struct _SharedMem {
    pub(crate) inner: platform::SharedMem,
    pub(crate) token: Uuid,
}

#[derive(Clone, Debug)]
pub struct SharedMemMap(Arc<_SharedMemMap>);

#[derive(Debug)]
struct _SharedMemMap {
    object: SharedMem,
    inner: platform::SharedMemMap,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Access {
    Read,
    ReadWrite,
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
        let inner = platform::SharedMem::new(size)?;
        let token = Uuid::new_v4();
        Ok(SharedMem(Arc::new(_SharedMem { inner, token })))
    }

    pub fn size(&self) -> usize { self.0.inner.size() }

    pub fn clone_with_access(&self, access: Access) -> io::Result<SharedMem> {
        let inner = self.0.inner.clone(access)?;
        Ok(SharedMem(Arc::new(_SharedMem { inner, token: self.0.token })))
    }

    pub fn map<R: RangeArgument<usize>>(&self, range: R, access: Access) -> io::Result<SharedMemMap> where {
        let inner = self.0.inner.map(range, access)?;
        Ok(SharedMemMap(Arc::new(_SharedMemMap {
            object: self.clone(),
            inner,
        })))
    }
}

impl SharedMemMap {
    pub unsafe fn pointer(&self) -> *mut u8 { self.0.inner.pointer() }
    pub fn len(&self) -> usize { self.0.inner.len() }
    pub fn access(&self) -> Access { self.0.inner.access() }
    pub fn offset(&self) -> usize { self.0.inner.offset() }

    pub(crate) fn token(&self) -> Uuid { self.0.object.0.token }
}

pub trait RangeArgument<T> {
    fn start(&self) -> Bound<&T>;
    fn end(&self) -> Bound<&T>;
}

impl<T> RangeArgument<T> for RangeFull {
    fn start(&self) -> Bound<&T> { Bound::Unbounded }
    fn end(&self) -> Bound<&T> { Bound::Unbounded }
}

impl<T> RangeArgument<T> for RangeFrom<T> {
    fn start(&self) -> Bound<&T> { Bound::Included(&self.start) }
    fn end(&self) -> Bound<&T> { Bound::Unbounded }
}

impl<T> RangeArgument<T> for RangeTo<T> {
    fn start(&self) -> Bound<&T> { Bound::Unbounded }
    fn end(&self) -> Bound<&T> { Bound::Excluded(&self.end) }
}

impl<T> RangeArgument<T> for Range<T> {
    fn start(&self) -> Bound<&T> { Bound::Included(&self.start) }
    fn end(&self) -> Bound<&T> { Bound::Excluded(&self.end) }
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
        let memory = memory.map(.., Access::ReadWrite).unwrap();
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
            let mapping = memory.map(.., Access::ReadWrite).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            slice[0..test_bytes.len()].copy_from_slice(test_bytes);
        }

        let _a = reactor.run(a.send(memory)).unwrap();
        let (message, _b) = reactor.run(b.into_future()).map_err(|(err, _)| err).unwrap();
        let memory: SharedMem = message.unwrap();

        unsafe {
            let mapping = memory.map(.., Access::Read).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            assert_eq!(&slice[0..test_bytes.len()], test_bytes);
        }
    }

    #[test]
    fn big_shm() {
        let memory = SharedMem::new(64 * 1024 * 1024).unwrap();
        let _mapping = memory.map(.., Access::ReadWrite).unwrap();
    }
}
