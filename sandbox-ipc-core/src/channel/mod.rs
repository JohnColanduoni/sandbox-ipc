pub mod raw;

use crate::platform;
use crate::resource::{Resource, ResourceRef, ResourceTransceiver, ResourceSerializer};

use std::{io, mem};
use std::marker::PhantomData;
use std::cell::RefCell;
use std::time::Duration;
use std::process::{Command, Child};

use serde::{Serialize};
use compio_core::queue::Registrar;

#[derive(Debug)]
pub struct Channel<T, R> {
    inner: platform::Channel,
    buffer: Vec<u8>,
    _phantom: PhantomData<(T, R)>,
}

// (Pre)Channel is Send and Sync even if T or R aren't
unsafe impl<T, R> Send for Channel<T, R> {}
unsafe impl<T, R> Sync for Channel<T, R> {}

#[derive(Debug)]
pub struct PreChannel<T, R> {
    inner: platform::PreChannel,
    _phantom: PhantomData<(T, R)>,
}

// (Pre)Channel is Send and Sync even if T or R aren't
unsafe impl<T, R> Send for PreChannel<T, R> {}
unsafe impl<T, R> Sync for PreChannel<T, R> {}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ChildChannelMetadata<T, R> {
    inner: platform::ChildChannelMetadata,
    _phantom: PhantomData<(T, R)>,
}

pub struct ChildChannelBuilder<T, R> {
    pub(crate) command: Command,
    pub(crate) resource_tx: bool,
    pub(crate) resource_rx: bool,
    pub(crate) timeout: Option<Duration>,
    _phantom: PhantomData<(T, R)>,
}

impl<T, R> Channel<T, R> {
    pub async fn send<'a>(&'a mut self, msg: &'a T) -> io::Result<()> where
        T: Serialize,
    {
        let mut resource_serializer = RefCell::new(ChannelSerializer {
            channel: Some(&mut self.inner),
            sender: None,
        });
        let buffer = &mut self.buffer;
        buffer.clear();
        ResourceSerializer::set(&resource_serializer, || {
            bincode::config().serialize_into(buffer, msg)
        })
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, format!("failed to serialize message: {}", err)))?;

        let resource_serializer = resource_serializer.get_mut();
        if let Some(channel) = resource_serializer.channel.take() {
            await!(channel.send(&self.buffer))
        } else if let Some(sender) = resource_serializer.sender.take() {
            await!(sender.finish(&self.buffer))
        } else {
            unreachable!()
        }
    }
}

impl<T, R> Clone for Channel<T, R> {
    fn clone(&self) -> Channel<T, R> {
        Channel {
            inner: self.inner.clone(),
            buffer: Vec::new(),
            _phantom: PhantomData,
        }
    }
}

struct ChannelSerializer<'a> {
    channel: Option<&'a mut platform::Channel>,
    sender: Option<platform::ChannelResourceSender<'a>>,
}

impl<'a> ChannelSerializer<'a> {
    fn ensure_sender(&mut self) -> io::Result<&mut platform::ChannelResourceSender<'a>> {
        if let Some(ref mut sender) = self.sender {
            Ok(sender)
        } else {
            let channel = self.channel.take().unwrap();
            self.sender = Some(channel.send_with_resources()?);
            Ok(self.sender.as_mut().unwrap())
        }
    }
}

impl<'a> ResourceSerializer for RefCell<ChannelSerializer<'a>> {
    fn serialize_owned(&self, resource: Resource, serializer: &mut erased_serde::Serializer) -> io::Result<()> {
        let mut inner = self.borrow_mut();
        let sender = inner.ensure_sender()?;
        sender.move_resource(resource.inner, serializer)
    }

    unsafe fn serialize_ref(&self, resource: ResourceRef, serializer: &mut erased_serde::Serializer) -> io::Result<()> {
        let mut inner = self.borrow_mut();
        let sender = inner.ensure_sender()?;
        sender.copy_resource(mem::transmute(resource.inner), serializer)
    }
}

impl<T, R> PreChannel<T, R> {
    pub fn pair() -> io::Result<(PreChannel<T, R>, PreChannel<R, T>)> {
        let (a, b) = platform::PreChannel::pair()?;
        Ok((
            PreChannel { inner: a, _phantom: PhantomData },
            PreChannel { inner: b, _phantom: PhantomData },
        ))
    }

    pub fn from_raw(channel: self::raw::PreChannel) -> io::Result<Self> {
        Ok(PreChannel {
            inner: channel.inner,
            _phantom: PhantomData,
        })
    }

    pub fn into_channel(self, queue: &Registrar) -> io::Result<Channel<T, R>> {
        Ok(Channel {
            inner: self.inner.into_channel(queue)?,
            buffer: Vec::new(),
            _phantom: PhantomData,
        })
    }

    pub fn into_resource_channel(self, queue: &Registrar, resource_tranceiver: ResourceTransceiver) -> io::Result<Channel<T, R>> {
        Ok(Channel {
            inner: self.inner.into_resource_channel(queue, resource_tranceiver.inner)?,
            buffer: Vec::new(),
            _phantom: PhantomData,
        })
    }
}

impl<T, R> ChildChannelMetadata<T, R> {
    pub fn into_pre_channel(self) -> io::Result<(PreChannel<T, R>, Option<ResourceTransceiver>)> {
        let (channel, transceiver) = self.inner.into_pre_channel()?;
        Ok((
            PreChannel {
                inner: channel,
                _phantom: PhantomData,
            },
            transceiver.map(|inner| ResourceTransceiver { inner }),
        ))
    }

    pub fn into_channel(self, queue: &Registrar) -> io::Result<Channel<T, R>> {
        let (channel, transceiver) = self.into_pre_channel()?;
        if let Some(transceiver) = transceiver {
            channel.into_resource_channel(queue, transceiver)
        } else {
            channel.into_channel(queue)
        }
    }
}

impl<T, R> ChildChannelBuilder<T, R> {
    pub fn new(command: Command) -> Self {
        ChildChannelBuilder {
            command,
            resource_tx: false,
            resource_rx: false,
            timeout: None,
            _phantom: PhantomData,
        }
    }

    pub fn enable_resource_tx(&mut self, enabled: bool) -> &mut Self {
        self.resource_tx = enabled;
        self
    }

    pub fn enable_resource_rx(&mut self, enabled: bool) -> &mut Self {
        self.resource_rx = enabled;
        self
    }

    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> &mut Self {
        self.timeout = timeout;
        self
    }

    pub fn spawn_pre_channel<RM>(&mut self, relay_metadata: RM) -> io::Result<(Child, PreChannel<T, R>, Option<ResourceTransceiver>)> where
        RM: for<'a> FnOnce(&'a mut Command, &'a ChildChannelMetadata<T, R>) -> io::Result<()>,
    {
        let (child, inner, transceiver) = platform::PreChannel::establish_with_child(self, |command, metadata| {
            relay_metadata(command, &ChildChannelMetadata {
                inner: metadata,
                _phantom: PhantomData,
            })
        })?;
        Ok((child, PreChannel { inner, _phantom: PhantomData }, transceiver.map(|inner| ResourceTransceiver { inner })))
    }

    pub fn spawn<RM>(&mut self, queue: &Registrar, relay_metadata: RM) -> io::Result<(Child, Channel<T, R>)> where
        RM: for<'a> FnOnce(&'a mut Command, &'a ChildChannelMetadata<T, R>) -> io::Result<()>,
    {
        let (child, channel, transceiver) = self.spawn_pre_channel(relay_metadata)?;
        let channel = if let Some(transceiver) = transceiver {
            channel.into_resource_channel(queue, transceiver)?
        } else {
            channel.into_channel(queue)?
        };
        Ok((child, channel))
    }
}