use crate::resource::{ResourceSerializeContext};

use std::ops::Deref;
use std::fs::File;
use std::os::unix::prelude::*;

use serde::{
    ser::{Serialize, Serializer, Error as SerError},
};
use sandbox_ipc_core::resource::{ResourceRef};
use sandbox_ipc_core::os::unix::*;

pub struct SendableFd(RawFd);

impl Drop for SendableFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0); }
    }
}

pub struct SendableFdRef<B>(pub B);

impl<F: AsRawFd, B: Deref<Target=F>> Serialize for SendableFdRef<B> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ResourceSerializeContext::serialize_with::<S, _>(|context| unsafe {
            let metadata = context.transmit_ref(ResourceRef::with_fd(self.0.deref()))
                .map_err(|err| S::Error::custom(format!("failed to transmit file descriptor: {}", err)))?;
            metadata.serialize(serializer)
        })
    }
}

pub fn transmit_file<S: Serializer>(file: &File, serializer: S) -> Result<S::Ok, S::Error> {
    ResourceSerializeContext::serialize_with::<S, _>(|context| unsafe {
        let metadata = context.transmit_ref(ResourceRef::with_fd(file))
            .map_err(|err| S::Error::custom(format!("failed to transmit file: {}", err)))?;
        metadata.serialize(serializer)
    })
}
