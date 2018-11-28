use crate::platform;

use std::{fmt};

/// An owned raw OS resources transmittable over a [`Channel`](crate::channel::Channel).
pub struct Resource {
    pub(crate) inner: platform::Resource,
}

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
/// `ResourceTransmitterExt::scm_rights()` function, which will transmit file descriptors directly over the channel.
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
/// `ResourceTransmitterExt::from_process_handle_insecure()` function may be used to create a resource transmitter.
/// 
/// If not, some trusted entity must be used to perform escrow of transmitted `HANDLE`s by holding process handles 
/// for each side of the connection (e.g. a supervisory process that manages another process's sandbox). In this case the
/// `ResourceTransmitterExt::from_escrow_agent()` function may be used.
/// 
/// # macOS
/// 
/// The macOS [`Channel`](crate::channel::Channel) is implemented via Mach ports, which can transmit rights to other Mach ports
/// inline in a familiar manner to the `SCM_RIGHTS` facility used with other Unix platforms. File descriptors are also supported 
/// using the `fileport_makeport`/`fileport_makefd` functions. The transmitter can be created by the
/// `ResourceTransmitterExt::mach()` function. 
pub struct ResourceTransmitter {
    pub(crate) inner: platform::ResourceTransmitter,
}

passthrough_debug!(Resource => inner);

impl<'a> fmt::Debug for ResourceRef<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.inner.fmt(f)
    }
}

passthrough_debug!(ResourceTransmitter => inner);
