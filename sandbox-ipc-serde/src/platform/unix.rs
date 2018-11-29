use crate::resource::{ResourceSerializeContext, ResourceDeserializeContext};

use std::ops::Deref;
use std::fs::File;
use std::os::unix::prelude::*;

use serde::{
    ser::{Serialize, Serializer, Error as SerError},
    de::{Deserialize, Deserializer, Error as DeError},
};
use sandbox_ipc_core::resource::{ResourceRef, ResourceMetadata};
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

pub fn recv_file<'de, D: Deserializer<'de>>(deserializer: D) -> Result<File, D::Error> {
    ResourceDeserializeContext::deserialize_with::<D, _, _>(|context| unsafe {
        let metadata = ResourceMetadata::deserialize(deserializer)?;
        let raw_fd = context.recv(metadata)
            .map_err(|err| D::Error::custom(format!("failed to receive file: {}", err)))?
            .into_raw_fd()
            .map_err(|resource| D::Error::custom(format!("improper resource type for file: {:?}", resource)))?;
        Ok(File::from_raw_fd(raw_fd))
    })
}
