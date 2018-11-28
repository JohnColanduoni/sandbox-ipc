#[path = "unix.rs"]
pub mod unix;

use std::{io, mem, slice};
use std::pin::{Pin, Unpin};
use std::time::Duration;
use std::os::raw::c_int;
use std::os::unix::prelude::*;

use byteorder::{ReadBytesExt, WriteBytesExt, NativeEndian};
use futures_core::{
    future::Future,
    task::{Poll, LocalWaker},
};
use futures_util::try_ready;
use compio_core::queue::Registrar;
use compio_core::os::macos::{RegistrarExt, PortRegistration, RawPort};
use mach_port::{Port, MsgBuffer, PortMoveMode, PortCopyMode, MsgDescriptorKindMut};

pub struct Channel {
    tx: Port,
    rx: Port,
    rx_registration: PortRegistration,
    resource_tranceiver: Option<ResourceTransceiver>,
}

pub struct PreChannel {
    tx: Port,
    rx: Port,
}

#[derive(Debug)]
pub enum Resource {
    Port(PortMoveMode, Port),
    Fd(ScopedFd),
}

#[derive(Debug)]
pub struct ScopedFd(c_int);

impl Drop for ScopedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0); }
    }
}

#[derive(Debug)]
pub enum ResourceRef<'a> {
    Port(PortCopyMode, &'a Port),
    Fd(c_int),
}

#[derive(Debug)]
pub enum ResourceTransceiver {
    Mach { tx: bool, rx: bool },
}

// Used only for serialization purposes
#[repr(u32)]
enum ResourceKind {
    Port,
    Fd,
}

impl Channel {
    pub fn send<'a>(&'a mut self, buffer: &'a [u8]) -> impl Future<Output=io::Result<()>> + Send + 'a {
        // TODO: reuse MsgBuffers
        ChannelResourceSender {
            channel: self,
            msg: MsgBuffer::new(),
        }.finish(buffer)
    }

    pub async fn recv<'a>(&'a mut self, buffer: &'a mut [u8]) -> io::Result<usize> {
        // TODO: reuse MsgBuffers
        let mut msg = MsgBuffer::new();
        await!(self.recv_into_msg_and_buffer(&mut msg, 0, buffer))
    }

    pub fn send_with_resources(&mut self) -> io::Result<ChannelResourceSender> {
        match self.resource_tranceiver {
            Some(ResourceTransceiver::Mach { tx: true, .. }) => {
                // TODO: reuse MsgBuffers
                Ok(ChannelResourceSender {
                    channel: self,
                    msg: MsgBuffer::new(),
                })
            },
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "this Channel does not support resource transmission")),
        }
    }

    pub fn recv_with_resources<'a, 'b>(&'a mut self, max_resources: usize, buffer: &'b mut [u8]) -> impl Future<Output=io::Result<ChannelResourceReceiver<'a>>> + Send + 'b where
        'a: 'b,
    {
        async move {
            match self.resource_tranceiver {
                Some(ResourceTransceiver::Mach { rx: true, .. }) => {},
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "this Channel does not support resource receiving")),
            }

            // TODO: reuse MsgBuffers
            let mut msg = MsgBuffer::new();
            let length = await!(self.recv_into_msg_and_buffer(&mut msg, max_resources, buffer))?;

            Ok(ChannelResourceReceiver {
                channel: self,
                msg,
                data_len: length,
            })
        }
    }

    fn recv_into_msg<'a>(&'a mut self, msg: &'a mut MsgBuffer) -> ChannelRecvFuture<'a> {
        ChannelRecvFuture {
            channel: self,
            msg,
        }
    }

    async fn recv_into_msg_and_buffer<'a>(&'a mut self, msg: &'a mut MsgBuffer, max_resources: usize, buffer: &'a mut [u8]) -> io::Result<usize> {
        msg.reserve_inline_data(mem::size_of::<usize>() + buffer.len());
        msg.reserve_descriptors(max_resources);
        await!(self.recv_into_msg(msg))?;

        if msg.inline_data().len() < mem::size_of::<usize>() {
            return Err(io::Error::new(io::ErrorKind::Other, "inline data in mach message too short"));
        }
        let (len_bytes, data) = msg.inline_data().split_at(mem::size_of::<usize>());
        let length = unsafe { *(len_bytes.as_ptr() as *const usize) };
        let source = data.get(..length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "inline data in mach message does not match declared length"))?;
        // TODO: The read will fail but the message will not be consumed if the reserved buffer is too small, but amortized allocation
        // of Vec may result in the MsgBuffer being bigger than the user provided buffer. Because of the space reserved for resource
        // descriptors we can't ensure this doesn't happen in general even if we add another size limit that we pass to the underlying
        // receive call. Perhaps we should keep the message around, allowing the user to try and read it again (normalizing the behavior
        // between the two capacity failures)?
        // TODO: With the above, should also check max_resources more strictly to help avoid conditional bugs
        let dest = buffer.get_mut(..length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "buffer too small for incomming message"))?;
        dest.copy_from_slice(source);

        Ok(length)
    }
}

pub struct ChannelResourceSender<'a> {
    channel: &'a mut Channel,
    msg: MsgBuffer,
}

impl<'a> ChannelResourceSender<'a> {
    pub fn move_resource<W: io::Write>(&mut self, resource: Resource, buffer: &mut W) -> io::Result<usize> {
        let (port, mode, kind) = match resource {
            Resource::Port(mode, port) => (port.into_raw_port(), mode, ResourceKind::Port),
            Resource::Fd(fd) => unsafe {
                let mut port: RawPort = 0;
                if fileport_makeport(fd.0, &mut port) < 0 {
                    let err = io::Error::last_os_error();
                    error!("failed to create Mach port from file descriptor: {}", err);
                    return Err(err);
                }
                (port, PortMoveMode::Send, ResourceKind::Fd)
            },
        };
        let descriptor_index = self.msg.descriptors().len();
        unsafe { self.msg.move_right_raw(mode, port); }
        buffer.write_u32::<NativeEndian>(descriptor_index as u32)?;
        buffer.write_u32::<NativeEndian>(kind as u32)?;
        Ok(mem::size_of::<u32>() * 2)
    }

    pub fn copy_resource<W: io::Write>(&mut self, resource: ResourceRef<'a>, buffer: &mut W) -> io::Result<usize> {
        let (port, mode, kind) = match resource {
            ResourceRef::Port(mode, port) => (port.as_raw_port(), mode, ResourceKind::Port),
            ResourceRef::Fd(fd) => unsafe {
                // TODO: can be made more efficient by using move_resource, since this doesn't close the original
                // fd
                let mut port: RawPort = 0;
                if fileport_makeport(fd, &mut port) < 0 {
                    let err = io::Error::last_os_error();
                    error!("failed to create Mach port from file descriptor: {}", err);
                    return Err(err);
                }
                (port, PortCopyMode::Send, ResourceKind::Fd)
            },
        };
        let descriptor_index = self.msg.descriptors().len();
        unsafe { self.msg.copy_right_raw(mode, port); }
        buffer.write_u32::<NativeEndian>(descriptor_index as u32)?;
        buffer.write_u32::<NativeEndian>(kind as u32)?;
        Ok(mem::size_of::<u32>() * 2)
    }

    pub fn finish<'b>(mut self, buffer: &'b [u8]) -> impl Future<Output=io::Result<()>> + 'a + 'b where
        'a: 'b,
    {
        let size = buffer.len();
        self.msg.reserve_inline_data(mem::size_of::<usize>() + buffer.len());
        self.msg.extend_inline_data(unsafe { slice::from_raw_parts(&size as *const usize as *const u8, mem::size_of::<usize>()) });
        self.msg.extend_inline_data(buffer);
        // Round data up to 4 byte boundary, due to mach message requirements
        match self.msg.inline_data().len() % mem::size_of::<u32>() {
            0 => {},
            c => {
                self.msg.extend_inline_data(&[0u8; mem::size_of::<u32>()][..(mem::size_of::<u32>() - c)]);
            },
        }
        ChannelSendFuture {
            channel: self.channel,
            msg: self.msg,
        }
    }
}

pub struct ChannelResourceReceiver<'a> {
    channel: &'a mut Channel,
    msg: MsgBuffer,
    data_len: usize,
}

impl<'a> ChannelResourceReceiver<'a> {
    pub fn data_len(&self) -> usize {
        self.data_len
    }

    pub fn recv_resource(&mut self, data: &[u8]) -> io::Result<Resource> {
        if data.len() != 2 * mem::size_of::<u32>() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "data too short to be a resource"));
        }
        let mut data = io::Cursor::new(data);
        let descriptor_index = data.read_u32::<NativeEndian>().unwrap();
        let kind = ResourceKind::from_u32(data.read_u32::<NativeEndian>().unwrap())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid resource kind in data"))?;
        
        let descriptor = self.msg.descriptors_mut().nth(descriptor_index as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "out of range descriptor index in data"))?;

        match descriptor.kind_mut() {
            MsgDescriptorKindMut::Port(descriptor) => {
                match kind {
                    ResourceKind::Port => {
                        let port = descriptor.take_port()?
                            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "resource already taken from message"))?;
                        unimplemented!()
                    },
                    ResourceKind::Fd => {
                        let port = descriptor.take_raw_port()
                            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "resource already taken from message"))?;
                        // FIXME: do we need to deallocate port ourselves or does fileport_makefd handle it?
                        let fd = unsafe { fileport_makefd(port) };
                        if fd < 0 {
                            // Not sure the error ends up in errno
                            let err = io::Error::last_os_error();
                            error!("fileport_makefd failed: {}", err);
                            return Err(err);
                        }
                        Ok(Resource::Fd(ScopedFd(fd)))
                    },
                }
            },
            _ => unimplemented!(),
        }
    }
}

pub struct ChannelSendFuture<'a> {
    channel: &'a mut Channel,
    msg: MsgBuffer,
}

impl<'a> Unpin for ChannelSendFuture<'a> {}

impl<'a> Future for ChannelSendFuture<'a> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, _waker: &LocalWaker) -> Poll<io::Result<()>> {
        // FIXME: implement async send with MACH_SEND_TIMEOUT and MACH_SEND_NOTIFY
        let this = &mut *self;
        this.channel.tx.send(&mut this.msg, None)?;
        Poll::Ready(Ok(()))
    }
}

pub struct ChannelRecvFuture<'a> {
    channel: &'a mut Channel,
    msg: &'a mut MsgBuffer,
}

impl<'a> Unpin for ChannelRecvFuture<'a> {}

impl<'a> Future for ChannelRecvFuture<'a> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, waker: &LocalWaker) -> Poll<io::Result<()>> {
        let this = &mut *self;
        try_ready!(this.channel.rx_registration.poll_recv_ready(waker));
        match this.channel.rx.recv(this.msg, Some(Duration::new(0, 0))) {
            Ok(()) => (),
            Err(ref err) if err.kind() == io::ErrorKind::TimedOut => {
                this.channel.rx_registration.clear_recv_ready(waker)?;
                return Poll::Pending;
            },
            Err(err) => return Poll::Ready(Err(err)),
        }
        Poll::Ready(Ok(()))
    }
}

impl PreChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let ltr = Port::new()?;
        let rtl = Port::new()?;
        let ltr_sender = ltr.make_sender()?;
        let rtl_sender = rtl.make_sender()?;

        Ok((
            PreChannel {
                tx: ltr_sender,
                rx: rtl,
            },
            PreChannel {
                tx: rtl_sender,
                rx: ltr,
            },
        ))
    }

    pub fn into_channel(self, queue: &Registrar) -> io::Result<Channel> {
        let rx_registration = queue.register_mach_port(self.rx.as_raw_port())?;
        Ok(Channel {
            tx: self.tx,
            rx: self.rx,
            rx_registration,
            resource_tranceiver: None,
        })
    }

    pub fn into_resource_channel(self, queue: &Registrar, resource_tranceiver: ResourceTransceiver) -> io::Result<Channel> {
        let rx_registration = queue.register_mach_port(self.rx.as_raw_port())?;
        Ok(Channel {
            tx: self.tx,
            rx: self.rx,
            rx_registration,
            resource_tranceiver: Some(resource_tranceiver),
        })
    }
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

impl self::unix::ResourceExt for crate::resource::Resource {
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

impl<'a> self::unix::ResourceRefExt<'a> for crate::resource::ResourceRef<'a> {
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

impl self::unix::ResourceTransceiverExt for crate::resource::ResourceTransceiver {
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

impl ResourceKind {
    fn from_u32(x: u32) -> Option<Self> {
        const PORT_U32: u32 = ResourceKind::Port as u32;
        const FD_U32: u32 = ResourceKind::Fd as u32;
        match x {
            PORT_U32 => Some(ResourceKind::Port),
            FD_U32 => Some(ResourceKind::Fd),
            _ => None,
        }
    }
}

extern "C" {
    fn fileport_makeport(fd: c_int, port: *mut RawPort) -> c_int;
    fn fileport_makefd(port: RawPort) -> c_int;
}