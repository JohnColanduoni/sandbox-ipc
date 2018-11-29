use crate::platform;
use crate::resource::{Resource, ResourceRef, ResourceTransceiver, ResourceMetadata};

use std::{io};
use std::time::Duration;
use std::process::{Command, Child};

use futures_core::future::Future;
use compio_core::queue::Registrar;

#[derive(Clone)]
pub struct Channel {
    pub(crate) inner: platform::Channel,
}

pub struct PreChannel {
    pub(crate) inner: platform::PreChannel,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ChildChannelMetadata {
    inner: platform::ChildChannelMetadata,
}

pub struct ChildChannelBuilder {
    pub(crate) command: Command,
    pub(crate) resource_tx: bool,
    pub(crate) resource_rx: bool,
    pub(crate) timeout: Option<Duration>,
}

impl PreChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = platform::PreChannel::pair()?;
        Ok((
            PreChannel { inner: a },
            PreChannel { inner: b },
        ))
    }

    pub fn into_channel(self, queue: &Registrar) -> io::Result<Channel> {
        Ok(Channel {
            inner: self.inner.into_channel(queue)?,
        })
    }

    pub fn into_resource_channel(self, queue: &Registrar, resource_tranceiver: ResourceTransceiver) -> io::Result<Channel> {
        Ok(Channel {
            inner: self.inner.into_resource_channel(queue, resource_tranceiver.inner)?,
        })
    }
}

passthrough_debug!(PreChannel => inner);

impl Channel {
    pub fn send<'a>(&'a mut self, buffer: &'a [u8]) -> impl Future<Output=io::Result<()>> + Send + 'a {
        self.inner.send(buffer)
    }

    pub fn recv<'a>(&'a mut self, buffer: &'a mut [u8]) -> impl Future<Output=io::Result<usize>> + Send + 'a {
        self.inner.recv(buffer)
    }

    /// Sends a message with [`Resource`]s attached.
    /// 
    /// Due to variations in how different platforms transmit OS resources between processes, this function requires
    /// significant support from the application layer. The returned [`ChannelResourceSender`] allows the caller to
    /// provide resources to be moved/copied to the destination, while producing data to be embeded into the message
    /// to facilitate reconstruction.
    pub fn send_with_resources<'a>(&'a mut self) -> io::Result<ChannelResourceSender<'a>> {
        Ok(ChannelResourceSender {
            inner: self.inner.send_with_resources()?,
        })
    }

    /// Receives a message with [`Resource`]s attached.
    /// 
    /// Due to variations in how different platforms transmit OS resources between processes, this function requires
    /// significant support from the application layer. The returned [`ChannelResourceReceiver`] allows the caller to
    /// reconstruct transmitted resources associated with data inside the message, which was provided by [`ChannelResourceSender`]
    /// on the other side of the connection.
    pub fn recv_with_resources<'a, 'b>(&'a mut self, max_resources: usize, buffer: &'b mut [u8]) -> impl Future<Output=io::Result<ChannelResourceReceiver<'a>>> + Send + 'b where
        'a: 'b,
    {
        async move {
            let inner = await!(self.inner.recv_with_resources(max_resources, buffer))?;
            Ok(ChannelResourceReceiver { inner })
        }
    }
}

passthrough_debug!(Channel => inner);

pub struct ChannelResourceSender<'a> {
    inner: platform::ChannelResourceSender<'a>,
}

impl<'a> ChannelResourceSender<'a> {
    /// Embeds a [`Resource`] in a message being constructed, transfering ownership to the receiver.
    /// 
    /// The data required to reconstruct the resource on the other side of the connection will be written to `serializer`. It is the
    /// caller's responsibility to ensure that the data is embedded in a message in a manner suitable for the receiver to call
    /// `ChannelResourceReceiver::recv_resource`.
    pub fn move_resource(&mut self, resource: Resource) -> io::Result<ResourceMetadata> {
        self.inner.move_resource(resource.inner).map(ResourceMetadata)
    }

    /// Embeds a [`Resource`] in a message being constructed, copying ownership to the receiver.
    /// 
    /// The data required to reconstruct the resource on the other side of the connection will be written to `serializer`. It is the
    /// caller's responsibility to ensure that the data is embedded in a message in a manner suitable for the receiver to call
    /// `ChannelResourceReceiver::recv_resource`.
    pub fn copy_resource(&mut self, resource: ResourceRef<'a>) -> io::Result<ResourceMetadata> {
        self.inner.copy_resource(resource.inner).map(ResourceMetadata)
    }

    /// Finishes sending the message
    pub fn finish<'b>(self, buffer: &'b [u8]) -> impl Future<Output=io::Result<()>> + 'b where
        'a: 'b,
    {
        self.inner.finish(buffer)
    }
}

pub struct ChannelResourceReceiver<'a> {
    inner: platform::ChannelResourceReceiver<'a>,
}

impl<'a> ChannelResourceReceiver<'a> {
    /// Obtains the number of bytes written to `buffer` passed to `Channel::recv_with_resources`.
    pub fn data_len(&self) -> usize {
        self.inner.data_len()
    }

    /// Extracts a [`Resource`] attached to the received message.
    /// 
    /// The data passed to this function must have been produced by a call to `ChannelResourceSender::move_resource` or 
    /// `ChannelResourceSender::copy_resource`. Each resource can only be extracted from the message once; attempting to
    /// extract a resource multiple times will cause an error.
    /// 
    /// On some platforms, failing to call `recv_resource` for all resources in the message may cause the resources to leak
    /// in certain circumstances.
    pub fn recv_resource(&mut self, metadata: ResourceMetadata) -> io::Result<Resource> {
        let inner = self.inner.recv_resource(metadata.0)?;
        Ok(Resource { inner })
    }
}

impl ChildChannelMetadata {
    pub fn into_pre_channel(self) -> io::Result<(PreChannel, Option<ResourceTransceiver>)> {
        let (channel, transceiver) = self.inner.into_pre_channel()?;
        Ok((
            PreChannel {
                inner: channel,
            },
            transceiver.map(|inner| ResourceTransceiver { inner }),
        ))
    }

    pub fn into_channel(self, queue: &Registrar) -> io::Result<Channel> {
        let (channel, transceiver) = self.into_pre_channel()?;
        if let Some(transceiver) = transceiver {
            channel.into_resource_channel(queue, transceiver)
        } else {
            channel.into_channel(queue)
        }
    }
}

impl ChildChannelBuilder {
    pub fn new(command: Command) -> Self {
        ChildChannelBuilder {
            command,
            resource_tx: false,
            resource_rx: false,
            timeout: None,
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

    pub fn spawn_pre_channel<RM>(&mut self, relay_metadata: RM) -> io::Result<(Child, PreChannel, Option<ResourceTransceiver>)> where
        RM: for<'a> FnOnce(&'a mut Command, &'a ChildChannelMetadata) -> io::Result<()>,
    {
        let (child, inner, transceiver) = platform::PreChannel::establish_with_child(self, |command, metadata| {
            relay_metadata(command, &ChildChannelMetadata {
                inner: metadata,
            })
        })?;
        Ok((child, PreChannel { inner }, transceiver.map(|inner| ResourceTransceiver { inner })))
    }

    pub fn spawn<RM>(&mut self, queue: &Registrar, relay_metadata: RM) -> io::Result<(Child, Channel)> where
        RM: for<'a> FnOnce(&'a mut Command, &'a ChildChannelMetadata) -> io::Result<()>,
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
