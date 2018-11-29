use super::{Resource, ResourceRef, ResourceTransceiver, ResourceMetadata, ResourceKind};
use crate::unix::{ScopedFd};

use std::{io, mem, slice, fmt};
use std::pin::{Pin, Unpin};
use std::sync::Arc;
use std::time::Duration;
use std::process::{Command, Child};
use std::ffi::CString;
use std::os::raw::{c_int, c_char};

use uuid::Uuid;
use rand::Rng;
use futures_core::{
    future::Future,
    task::{Poll, LocalWaker},
};
use futures_util::try_ready;
use compio_core::queue::Registrar;
use compio_core::os::macos::{RegistrarExt, PortRegistration, RawPort};
use mach_port::{Port, MsgBuffer, PortMoveMode, PortCopyMode, MsgDescriptorKindMut};
use mach_core::{mach_kern_call};

// TODO: notification of broken pipe (i.e. other side of channel dropped)
#[derive(Clone)]
pub struct Channel {
    inner: Arc<_Channel>,
    rx_registration: PortRegistration,
}

#[derive(Debug)]
struct _Channel {
    tx: Port,
    rx: Port,
    resource_tranceiver: Option<ResourceTransceiver>,
}

#[derive(Debug)]
pub struct PreChannel {
    tx: Port,
    rx: Port,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ChildChannelMetadata {
    bootstrap_name: String,
    token: [u8; 32],
    resource_rx: bool,
    resource_tx: bool,
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
        match self.inner.resource_tranceiver {
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
            match self.inner.resource_tranceiver {
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

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Channel")
            .field("tx", &self.inner.tx)
            .field("rx", &self.inner.rx)
            .finish()
    }
}

pub struct ChannelResourceSender<'a> {
    channel: &'a mut Channel,
    msg: MsgBuffer,
}

impl<'a> ChannelResourceSender<'a> {
    pub fn move_resource(&mut self, resource: Resource) -> io::Result<ResourceMetadata> {
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
        let descriptor_index = self.msg.descriptors().len() as u32;
        unsafe { self.msg.move_right_raw(mode, port); }
        Ok(ResourceMetadata { descriptor_index, kind })
    }

    pub fn copy_resource(&mut self, resource: ResourceRef<'a>) -> io::Result<ResourceMetadata> {
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
        let descriptor_index = self.msg.descriptors().len() as u32;
        unsafe { self.msg.copy_right_raw(mode, port); }
        Ok(ResourceMetadata { descriptor_index, kind })
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

    pub fn recv_resource(&mut self, metadata: ResourceMetadata) -> io::Result<Resource> {
        let ResourceMetadata { descriptor_index, kind } = metadata;

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
        this.channel.inner.tx.send(&mut this.msg, None)?;
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
        match this.channel.inner.rx.recv(this.msg, Some(Duration::new(0, 0))) {
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
            inner: Arc::new(_Channel {
                tx: self.tx,
                rx: self.rx,
                resource_tranceiver: None,
            }),
            rx_registration,
        })
    }

    pub fn into_resource_channel(self, queue: &Registrar, resource_tranceiver: ResourceTransceiver) -> io::Result<Channel> {
        let rx_registration = queue.register_mach_port(self.rx.as_raw_port())?;
        Ok(Channel {
            inner: Arc::new(_Channel {
                tx: self.tx,
                rx: self.rx,
                resource_tranceiver: Some(resource_tranceiver),
            }),
            rx_registration,
        })
    }

    pub fn establish_with_child<RM>(builder: &mut crate::channel::ChildChannelBuilder, relay_metadata: RM) -> io::Result<(Child, Self, Option<ResourceTransceiver>)> where
        RM: for<'a> FnOnce(&'a mut Command, ChildChannelMetadata) -> io::Result<()>,
    {
        // The process for launching a child that has a mach channel with us is as follows:
        //
        // * Create a port and register the send right with the bootstrap server, under a random name.
        // * Generate a random token.
        // * Launch the process, somehow passing the random service name and random token to it.
        // * The process gets a send right for our temporary port from the bootstrap server.
        // * It sends a message containing the token as inline data, and containing a send right
        //   to a port it controls and a receive right to which it has a send right.
        // * We validate the token, and create a channel with the rights received from the child process.
        //
        // The token exists because in theory another process could race out child to connect to our globally visible
        // bootstrap service, and then trick us into communicating with it instead of our child process.
        let bootstrap_name = format!("rust.sandbox-ipc-core.child.{}", Uuid::new_v4());
        let bootstrap_name_cstr = CString::new(bootstrap_name.clone()).unwrap();
        let init_port = Port::new()?;
        let init_port_sender = init_port.make_sender()?;
        unsafe {
            let mut bootstrap_port = 0;
            mach_kern_call!(log: mach_sys::task_get_special_port(
                mach_sys::mach_task_self(),
                mach_sys::TASK_BOOTSTRAP_PORT as _,
                &mut bootstrap_port,
            ), "failed to retreive bootstrap port: {}")?;

            debug!("registering temporary mach bootstrap service {:?}", bootstrap_name);
            mach_kern_call!(log: bootstrap_register2(
                bootstrap_port,
                bootstrap_name_cstr.as_ptr(),
                init_port_sender.as_raw_port(),
                0,
            ), "failed to register port for child communication with bootstrap server: {}")?;
        }

        let metadata = ChildChannelMetadata {
            bootstrap_name,
            token: rand::thread_rng().gen(),
            resource_tx: builder.resource_rx,
            resource_rx: builder.resource_tx,
        };
        let token = metadata.token.clone();
        relay_metadata(&mut builder.command, metadata)?;

        let child = builder.command.spawn()?;

        let mut msg = MsgBuffer::new();
        msg.reserve_descriptors(2);
        msg.reserve_inline_data(token.len());
        init_port.recv(&mut msg, builder.timeout)?;

        if msg.inline_data() != &token {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "token in message received on advertised port did not match the one passed to child process"));
        }

        let mut descriptors = msg.descriptors_mut();
        let tx_descriptor = descriptors.next()
            .and_then(|desc| if let MsgDescriptorKindMut::Port(desc) = desc.kind_mut() { Some(desc) } else { None })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing tx descriptor in message from child process"))?;
        let rx_descriptor = descriptors.next()
            .and_then(|desc| if let MsgDescriptorKindMut::Port(desc) = desc.kind_mut() { Some(desc) } else { None })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing rx descriptor in message from child process"))?;
        let tx = tx_descriptor.take_port()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing port in tx descriptor in message from child process"))?;
        let rx = rx_descriptor.take_port()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing port in rx descriptor in message from child process"))?;

        let resource_transceiver = match (builder.resource_tx, builder.resource_rx) {
            (false, false) => None,
            (tx, rx) => Some(ResourceTransceiver::Mach { tx, rx }),
        };

        Ok((child, PreChannel { tx, rx }, resource_transceiver))
    }
}

impl ChildChannelMetadata {
    pub fn into_pre_channel(self) -> io::Result<(PreChannel, Option<ResourceTransceiver>)> {
        let bootstrap_name_cstr = CString::new(self.bootstrap_name)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("invalid bootstrap service name: {}", err)))?;
        let init_port = unsafe {
            let mut bootstrap_port = 0;
            mach_kern_call!(log: mach_sys::task_get_special_port(
                mach_sys::mach_task_self(),
                mach_sys::TASK_BOOTSTRAP_PORT as _,
                &mut bootstrap_port,
            ), "failed to retreive bootstrap port: {}")?;

            let mut init_port = 0;
            mach_kern_call!(log: bootstrap_look_up2(
                bootstrap_port,
                bootstrap_name_cstr.as_ptr(),
                &mut init_port,
                0,
                0,
            ), "failed to register port for child communication with bootstrap server: {}")?;
            Port::from_raw_port(init_port)?
        };

        let (ours, theirs) = PreChannel::pair()?;

        let mut msg = MsgBuffer::new();
        msg.move_right(PortMoveMode::Send, theirs.tx);
        msg.move_right(PortMoveMode::Receive, theirs.rx);
        msg.extend_inline_data(&self.token);

        init_port.send(&mut msg, None)?;

        let resource_transceiver = match (self.resource_tx, self.resource_rx) {
            (false, false) => None,
            (tx, rx) => Some(ResourceTransceiver::Mach { tx, rx }),
        };

        Ok((ours, resource_transceiver))
    }
}

extern "C" {
    fn bootstrap_register2(bp: RawPort, service_name: *const c_char, sp: RawPort, flags: u64) -> mach_sys::kern_return_t;
    fn bootstrap_look_up2(bp: RawPort, service_name: *const c_char, sp: *mut RawPort, target_pid: libc::pid_t, flags: u64) -> mach_sys::kern_return_t;

    fn fileport_makeport(fd: c_int, port: *mut RawPort) -> c_int;
    fn fileport_makefd(port: RawPort) -> c_int;
}
