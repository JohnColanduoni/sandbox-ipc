use crate::unix::{ScopedFd};

use std::{mem};
use std::os::raw::{c_int};
use std::os::unix::prelude::*;

use mach_port::{Port, PortMoveMode, PortCopyMode};


#[derive(Debug)]
pub enum Resource {
    Port(PortMoveMode, Port),
    Fd(ScopedFd),
}

#[derive(Clone, Debug)]
pub enum ResourceRef<'a> {
    Port(PortCopyMode, &'a Port),
    Fd(c_int),
}

#[derive(Debug)]
pub enum ResourceTransceiver {
    Mach { tx: bool, rx: bool },
}

pub trait ResourceExt: Sized {
    fn from_mach_port(mode: PortMoveMode, port: Port) -> Self;
    fn as_mach_port(&self) -> Option<&Port>;
    fn into_mach_port(self) -> Result<Port, Self>;
}

impl ResourceExt for crate::resource::Resource {
    fn from_mach_port(mode: PortMoveMode, port: Port) -> Self {
        crate::resource::Resource { inner: Resource::Port(mode, port) }
    }

    fn as_mach_port(&self) -> Option<&Port> {
        match &self.inner {
            Resource::Port(_, port) => Some(port),
            _ => None,
        }
    }

    fn into_mach_port(self) -> Result<Port, Self> {
        match self.inner {
            Resource::Port(_, port) => Ok(port),
            _ => Err(self),
        }
    }
}

impl crate::unix::ResourceExt for crate::resource::Resource {
    fn from_fd<F: IntoRawFd>(fd: F) -> Self {
        unsafe { Self::from_raw_fd(fd.into_raw_fd()) }
    }

    fn as_raw_fd(&self) -> Option<c_int> {
        if let Resource::Fd(fd) = &self.inner {
            Some(fd.0)
        } else {
            None
        }
    }

    fn into_raw_fd(self) -> Result<c_int, Self> {
        if let Resource::Fd(fd) = self.inner {
            let fd_int = fd.0;
            mem::forget(fd);
            Ok(fd_int)
        } else {
            Err(self)
        }
    }
}

impl FromRawFd for crate::resource::Resource {
    unsafe fn from_raw_fd(fd: c_int) -> Self {
        crate::resource::Resource { inner: Resource::Fd(ScopedFd(fd)) }
    }
}

pub trait ResourceRefExt<'a> {
    fn with_mach_port(mode: PortCopyMode, port: &'a Port) -> Self;
    fn as_mach_port(&self) -> Option<&Port>;
}

impl<'a> ResourceRefExt<'a> for crate::resource::ResourceRef<'a> {
    fn with_mach_port(mode: PortCopyMode, port: &'a Port) -> Self {
        crate::resource::ResourceRef { inner: ResourceRef::Port(mode, port) }
    }
    fn as_mach_port(&self) -> Option<&Port> {
        if let ResourceRef::Port(_, port) = self.inner {
            Some(port)
        } else {
            None
        }
    }
}

impl<'a> crate::unix::ResourceRefExt<'a> for crate::resource::ResourceRef<'a> {
    fn with_fd<F: AsRawFd>(fd: &'a F) -> Self {
        unsafe { Self::with_raw_fd(fd.as_raw_fd()) }
    }
    unsafe fn with_raw_fd(fd: c_int) -> Self {
        crate::resource::ResourceRef { inner: ResourceRef::Fd(fd) }
    }
    fn as_raw_fd(&self) -> Option<c_int> {
        if let ResourceRef::Fd(fd) = self.inner {
            Some(fd)
        } else {
            None
        }
    }
}

pub trait ResourceTransceiverExt {
    fn mach() -> Self;
    fn mach_tx_only() -> Self;
    fn mach_rx_only() -> Self;
}

impl ResourceTransceiverExt for crate::resource::ResourceTransceiver {
    fn mach() -> Self {
        crate::resource::ResourceTransceiver {
            inner: ResourceTransceiver::Mach { tx: true, rx: true },
        }
    }
    fn mach_tx_only() -> Self {
        crate::resource::ResourceTransceiver {
            inner: ResourceTransceiver::Mach { tx: true, rx: false },
        }
    }
    fn mach_rx_only() -> Self {
        crate::resource::ResourceTransceiver {
            inner: ResourceTransceiver::Mach { tx: false, rx: true },
        }
    }
}

impl crate::unix::ResourceTransceiverExt for crate::resource::ResourceTransceiver {
    fn inline() -> Self {
        Self::mach()
    }
    fn inline_tx_only() -> Self {
        Self::mach_tx_only()
    }
    fn inline_rx_only() -> Self {
        Self::mach_rx_only()
    }
}