use crate::platform;

use std::{io, mem};
use std::borrow::Borrow;
use std::cell::Cell;
use std::ptr::NonNull;
use std::fs::File;

use serde::{
    ser::{Serialize, Serializer, Error as SerError},
};
use sandbox_ipc_core::resource::{Resource, ResourceRef, ResourceMetadata};

pub trait ResourceSerializeContext {
    fn transmit_owned(&self, resource: Resource) -> io::Result<ResourceMetadata>;

    /// 
    /// 
    /// Callers must ensure the [`ResourceRef`] is not dropped until the context has finished transmission.
    /// This generally means the context is expected to maintain a reference to the data being serialized
    /// until transmission is complete.
    unsafe fn transmit_ref(&self, resource: ResourceRef) -> io::Result<ResourceMetadata>;
}

thread_local!(static CURRENT_RESOURCE_SERIALIZE_CONTEXT: Cell<Option<NonNull<ResourceSerializeContext>>> = Cell::new(None));

// I'd love to use scoped-tls for all of these but it doesn't support trait objects :(
impl ResourceSerializeContext {
    pub fn set<'a, F, R>(context: &'a dyn ResourceSerializeContext, f: F) -> R where
        F: FnOnce() -> R,
    {
        let guard = ResourceSerializeContextGuard(CURRENT_RESOURCE_SERIALIZE_CONTEXT.with(|cell| {
            cell.replace(unsafe { Some(mem::transmute(context)) })
        }));
        let result = f();
        mem::drop(guard);
        result
    }

    pub fn serialize_with<S, F>(f: F) -> Result<S::Ok, S::Error> where
        F: for<'a> FnOnce(&'a dyn ResourceSerializeContext) -> Result<S::Ok, S::Error>,
        S: Serializer,
    {
        if let Some(pointer) = CURRENT_RESOURCE_SERIALIZE_CONTEXT.with(|cell| cell.get()) {
            f(unsafe { pointer.as_ref() })
        } else {
            Err(S::Error::custom("attempted to serialize an OS resource outside of an appropriate context (e.g. a Channel::send)"))
        }
    }
}

struct ResourceSerializeContextGuard(Option<NonNull<ResourceSerializeContext>>);

impl Drop for ResourceSerializeContextGuard {
    fn drop(&mut self) {
        CURRENT_RESOURCE_SERIALIZE_CONTEXT.with(|cell| cell.set(self.0.take()));
    }
}

pub struct SendableFile<B>(pub B);

impl<B: Borrow<File>> Serialize for SendableFile<B> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        platform::transmit_file(self.0.borrow(), serializer)
    }
}