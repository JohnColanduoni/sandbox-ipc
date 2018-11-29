use crate::platform;
use crate::resource::{Resource, ResourceRef, ResourceTransceiver};

use std::{io};

use serde::{Serializer, Deserializer};
use futures_core::future::Future;
use compio_core::queue::Registrar;

pub struct Channel {
    pub(crate) inner: platform::Channel,
}

pub struct PreChannel {
    pub(crate) inner: platform::PreChannel,
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

pub struct ChannelResourceSender<'a> {
    inner: platform::ChannelResourceSender<'a>,
}

impl<'a> ChannelResourceSender<'a> {
    /// Embeds a [`Resource`] in a message being constructed, transfering ownership to the receiver.
    /// 
    /// The data required to reconstruct the resource on the other side of the connection will be written to `serializer`. It is the
    /// caller's responsibility to ensure that the data is embedded in a message in a manner suitable for the receiver to call
    /// `ChannelResourceReceiver::recv_resource`.
    pub fn move_resource<S: Serializer>(&mut self, resource: Resource, serializer: S) -> io::Result<()> {
        self.inner.move_resource(resource.inner, serializer)
    }

    /// Embeds a [`Resource`] in a message being constructed, copying ownership to the receiver.
    /// 
    /// The data required to reconstruct the resource on the other side of the connection will be written to `serializer`. It is the
    /// caller's responsibility to ensure that the data is embedded in a message in a manner suitable for the receiver to call
    /// `ChannelResourceReceiver::recv_resource`.
    pub fn copy_resource<S: Serializer>(&mut self, resource: ResourceRef<'a>, serializer: S) -> io::Result<()> {
        self.inner.copy_resource(resource.inner, serializer)
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
    pub fn recv_resource<'de, D: Deserializer<'de>>(&mut self, deserializer: D) -> io::Result<Resource> {
        let inner = self.inner.recv_resource(deserializer)?;
        Ok(Resource { inner })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::join;
    use compio::local::LocalExecutor;

    #[test]
    fn make_prechannel() {
        let (_a, _b) = PreChannel::pair().unwrap();
    }


    #[cfg(unix)]
    #[test]
    fn send_fd() {
        use std::{fs};
        use std::io::{Read, Write, Seek, SeekFrom};
        use std::os::unix::prelude::*;
        

        use crate::os::unix::*;

        let mut executor = LocalExecutor::new().unwrap();
        let (a, b) = PreChannel::pair().unwrap();
        let mut a = a.into_resource_channel(&executor.registrar(), ResourceTransceiver::inline_tx_only()).unwrap();
        let mut b = b.into_resource_channel(&executor.registrar(), ResourceTransceiver::inline_rx_only()).unwrap();

        let message = b"Hello world!";

        executor.block_on(async {
            let recv = async {
                let mut buffer = vec![0u8; 1024];
                let mut receiver = await!(b.recv_with_resources(1, &mut buffer)).unwrap();
                let resource = receiver.recv_resource(&mut serde_json::Deserializer::from_slice(&buffer[..receiver.data_len()])).unwrap();
                let mut f = unsafe { fs::File::from_raw_fd(resource.into_raw_fd().unwrap()) };
                f.seek(SeekFrom::Start(0)).unwrap();
                let mut file_buffer = Vec::new();
                f.read_to_end(&mut file_buffer).unwrap();
                assert_eq!(message, &*file_buffer);
            };

            let send = async {
                let mut f = tempfile::tempfile().unwrap();
                f.write_all(message).unwrap();
                let mut buffer = Vec::new();
                let mut sender = a.send_with_resources().unwrap();
                sender.move_resource(Resource::from_fd(f), &mut serde_json::Serializer::new(&mut buffer)).unwrap();
                await!(sender.finish(&buffer)).unwrap();
            };

            join!(recv, send);
        });
    }
}