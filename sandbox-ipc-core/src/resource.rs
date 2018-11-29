use crate::platform;

use std::{fmt, mem, io};
use std::cell::Cell;
use std::io::Write;

use serde::ser::{Error as SerError};

/// An owned raw OS resources transmittable over a [`Channel`](crate::channel::Channel).
pub struct Resource {
    pub(crate) inner: platform::Resource,
}

#[derive(Clone)]
/// A reference to a raw OS resources transmittable over a [`Channel`](crate::channel::Channel).
pub struct ResourceRef<'a> {
    pub(crate) inner: platform::ResourceRef<'a>,
}

/// Allows transmission of OS resources (files, etc.) over [`Channel`](crate::channel::Channel)s.
/// 
/// Some platforms (e.g. Windows) do not have a facillity for directly transmitting OS resources over
/// IPC channels. For these platforms, the resources must be transmitted by another means are reunited
/// with the data in the destination.
/// 
/// # Platform Specific Notes
/// 
/// ## non-macOS Unix
/// 
/// No out of line transmission of resources is necessary, so a transmitter can be created via the platform specific
/// `ResourceTransceiverExt::scm_rights()` function, which will transmit file descriptors directly over the channel.
/// 
/// ## Windows
/// 
/// The only method available for transmitting Windows `HANDLE`s between processes is `DuplicateHandle`. This method
/// requires a process handle with the `PROCESS_DUP_HANDLE` right. Unfortunately, this right also gives the holder the
/// ability to use `DuplicateHandle` to pull handles (including psuedo-handles like `GetCurrentProcess()`) out of the
/// target process, effectively giving full control over the target process (including the ability to edit arbitrary
/// process memory remotely or move thread's instruction pointers).
/// 
/// If this behavior is acceptable (e.g. if you are not using the [`Channel`](crate::channel::Channel) across a security
/// boundary, or if the sender is strictly more privileged than the receiver), the 
/// `ResourceTransceiverExt::from_process_handle_insecure()` function may be used to create a resource transmitter.
/// 
/// If not, some trusted entity must be used to perform escrow of transmitted `HANDLE`s by holding process handles 
/// for each side of the connection (e.g. a supervisory process that manages another process's sandbox). In this case the
/// `ResourceTransceiverExt::from_escrow_agent()` function may be used.
/// 
/// # macOS
/// 
/// The macOS [`Channel`](crate::channel::Channel) is implemented via Mach ports, which can transmit rights to other Mach ports
/// inline in a familiar manner to the `SCM_RIGHTS` facility used with other Unix platforms. File descriptors are also supported 
/// using the `fileport_makeport`/`fileport_makefd` functions. The transmitter can be created by the
/// `ResourceTransceiverExt::mach()` function. 
pub struct ResourceTransceiver {
    pub(crate) inner: platform::ResourceTransceiver,
}

/// Trait that facilitates the serialization of data containing OS resources by `serde`.
/// 
/// A `ResourceSerializer` can be set in a (thread-local) context to allow [`Serialize`](serde::ser::Serialize) 
/// implementations to transfer OS resources out of band, while embeding the necessary metadata for reconstruction
/// inside the serialized data. In particular, [`Channel`](crate::channel::Channel) provides an appropriate implementation
/// when sending a message.
pub trait ResourceSerializer {
    fn serialize_owned(&self, resource: Resource, serializer: &mut erased_serde::Serializer) -> Result<(), erased_serde::Error>;
    fn serialize_ref(&self, resource: ResourceRef, serializer: &mut erased_serde::Serializer) -> Result<(), erased_serde::Error>;
}

/// Trait that facilitates the deserialization of data containing OS resources by `serde`.
/// 
/// A `ResourceDeserializer` can be set in a (thread-local) context to allow [`Deserialize`](serde::de::Deserialize) 
/// implementations to transfer OS resources out of band, while extracting the necessary metadata for reconstruction
/// from the serialized data. In particular, [`Channel`](crate::channel::Channel) provides an appropriate implementation
/// when receiving a message.
pub trait ResourceDeserializer {
    fn deserialize(&self, deserializer: &mut erased_serde::Deserializer) -> Result<Resource, erased_serde::Error>;
}

passthrough_debug!(Resource => inner);

impl<'a> fmt::Debug for ResourceRef<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.inner.fmt(f)
    }
}

passthrough_debug!(ResourceTransceiver => inner);

thread_local!(static CURRENT_RESOURCE_SERIALIZER: Cell<Option<*const dyn ResourceSerializer>> = Cell::new(None));

impl ResourceSerializer {
    #[inline]
    pub fn set<F, R>(serializer: &dyn ResourceSerializer, f: F) -> R where
        F: FnOnce() -> R,
    {
        unsafe {
            CURRENT_RESOURCE_SERIALIZER.with(|r| {
                let previous = r.replace(Some(mem::transmute_copy::<_, &(dyn ResourceSerializer + 'static)>(&serializer) as *const dyn ResourceSerializer));
                let guard = ResourceSerializerGuard(previous);
                let result = f();
                mem::drop(guard);
                result
            })
        }
    }

    #[inline]
    pub fn has_current() -> bool {
        CURRENT_RESOURCE_SERIALIZER.with(|r| r.get().is_some())
    }

    #[inline]
    pub fn serialize_with_current<F, R, E>(f: F) -> Result<R, E> where
        F: for<'a> FnOnce(&'a ResourceSerializer) -> Result<R, E>,
        E: serde::ser::Error,
    {
        CURRENT_RESOURCE_SERIALIZER.with(|r| {
            if let Some(serializer) = r.get() {
                unsafe { f(&*serializer) }
            } else {
                Err(E::custom("attempted to serialize OS resource in invalid context"))
            }
        })
    }
}

struct ResourceSerializerGuard(Option<*const dyn ResourceSerializer>);

impl Drop for ResourceSerializerGuard {
    fn drop(&mut self) {
        CURRENT_RESOURCE_SERIALIZER.with(|r| {
            r.set(self.0.take());
        });
    }
}
