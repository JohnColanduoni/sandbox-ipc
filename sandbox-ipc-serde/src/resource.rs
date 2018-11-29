use crate::platform;

use std::{io, mem};
use std::borrow::Borrow;
use std::cell::Cell;
use std::ptr::NonNull;
use std::fs::File;

use serde::{
    ser::{Serialize, Serializer, Error as SerError},
    de::{Deserialize, Deserializer, Error as DeError},
};
use sandbox_ipc_core::resource::{Resource, ResourceRef, ResourceMetadata};

// TODO: implementations need a way to handle repeated attempts to serialize the same resource well. This can happen if
// e.g. bincode does length checks.
pub trait ResourceSerializeContext {
    fn transmit_owned(&self, resource: Resource) -> io::Result<ResourceMetadata>;

    /// 
    /// 
    /// Callers must ensure the [`ResourceRef`] is not dropped until the context has finished transmission.
    /// This generally means the context is expected to maintain a reference to the data being serialized
    /// until transmission is complete.
    unsafe fn transmit_ref(&self, resource: ResourceRef) -> io::Result<ResourceMetadata>;
}

pub trait ResourceDeserializeContext {
    fn recv(&self, metadata: ResourceMetadata) -> io::Result<Resource>;
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

thread_local!(static CURRENT_RESOURCE_DESERIALIZE_CONTEXT: Cell<Option<NonNull<ResourceDeserializeContext>>> = Cell::new(None));

// I'd love to use scoped-tls for all of these but it doesn't support trait objects :(
impl ResourceDeserializeContext {
    pub fn set<'a, F, R>(context: &'a dyn ResourceDeserializeContext, f: F) -> R where
        F: FnOnce() -> R,
    {
        let guard = ResourceDeserializeContextGuard(CURRENT_RESOURCE_DESERIALIZE_CONTEXT.with(|cell| {
            cell.replace(unsafe { Some(mem::transmute(context)) })
        }));
        let result = f();
        mem::drop(guard);
        result
    }

    pub fn deserialize_with<'de, D, T, F>(f: F) -> Result<T, D::Error> where
        F: for<'a> FnOnce(&'a dyn ResourceDeserializeContext) -> Result<T, D::Error>,
        D: Deserializer<'de>,
    {
        if let Some(pointer) = CURRENT_RESOURCE_DESERIALIZE_CONTEXT.with(|cell| cell.get()) {
            f(unsafe { pointer.as_ref() })
        } else {
            Err(D::Error::custom("attempted to deserialize an OS resource outside of an appropriate context (e.g. a Channel::recv)"))
        }
    }
}

struct ResourceDeserializeContextGuard(Option<NonNull<ResourceDeserializeContext>>);

impl Drop for ResourceDeserializeContextGuard {
    fn drop(&mut self) {
        CURRENT_RESOURCE_DESERIALIZE_CONTEXT.with(|cell| cell.set(self.0.take()));
    }
}

pub struct SendableFile<B>(pub B);

impl<B: Borrow<File>> Serialize for SendableFile<B> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        platform::transmit_file(self.0.borrow(), serializer)
    }
}

impl<'de> Deserialize<'de> for SendableFile<File> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(SendableFile(platform::recv_file(deserializer)?))
    }
}