use crate::resource::{ResourceSerializeContext};

use std::fs::File;

use serde::{
    ser::{Serialize, Serializer, Error as SerError},
};
use sandbox_ipc_core::resource::{ResourceRef};
use sandbox_ipc_core::os::unix::*;

pub fn transmit_file<S: Serializer>(file: &File, serializer: S) -> Result<S::Ok, S::Error> {
    ResourceSerializeContext::serialize_with::<S, _>(|context| unsafe {
        let metadata = context.transmit_ref(ResourceRef::with_fd(file))
            .map_err(|err| S::Error::custom(format!("failed to transmit file: {}", err)))?;
        metadata.serialize(serializer)
    })
}
