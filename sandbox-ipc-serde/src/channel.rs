use crate::resource::{ResourceSerializeContext, ResourceDeserializeContext};

use std::{io, mem};
use std::cell::RefCell;

use serde::{
    ser::{Serialize},
    de::{DeserializeOwned},
};
use sandbox_ipc_core::channel::{Channel as RawChannel, ChannelResourceSender, ChannelResourceReceiver};
use sandbox_ipc_core::resource::{Resource, ResourceRef, ResourceMetadata};

#[derive(Debug)]
pub struct Channel {
    inner: RawChannel,
    recv_buffer_len: usize,
    buffer: Vec<u8>,
}

const CHANNEL_MSG_MAX_RESOURCES: usize = 32;
const CHANNEL_MSG_DEFAULT_BUFFER_LEN: usize = 4096;

impl Clone for Channel {
    fn clone(&self) -> Channel {
        Channel {
            inner: self.inner.clone(),
            recv_buffer_len: self.recv_buffer_len.clone(),
            buffer: Vec::new(),
        }
    }
}

impl Channel {
    pub fn from_raw(channel: RawChannel) -> io::Result<Self> {
        Ok(Channel {
            inner: channel,
            recv_buffer_len: CHANNEL_MSG_DEFAULT_BUFFER_LEN,
            buffer: Vec::new(),
        })
    }

    /// Sets the size of the receive buffer for deserialization.
    pub fn set_recv_buffer_len(&mut self, len: usize) {
        self.recv_buffer_len = len;
    }

    pub fn recv_buffer_len(&mut self) -> usize {
        self.recv_buffer_len
    }

    pub async fn send<'a, M>(&'a mut self, message: &'a M) -> io::Result<()> where
        M: Serialize,
    {
        let sender = SerContext(RefCell::new(self.inner.send_with_resources()?));
        debug_assert!(self.buffer.is_empty());
        let buffer = &mut self.buffer;
        ResourceSerializeContext::set(&sender, || {
            bincode::serialize_into(buffer, message)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, format!("serialization failed: {}", err)))
        })?;
        await!(sender.0.into_inner().finish(&self.buffer))?;
        self.buffer.clear();
        Ok(())
    }

    pub async fn recv<'a, M>(&'a mut self) -> io::Result<M> where
        M: DeserializeOwned,
    {
        // TODO: Need to normalize the handling of truncations on different platforms. Windows and macOS allow receives to
        // not truncate the incomming message and instead provide the buffer size required; AF_UNIX w/ SOCK_SEQPACKET does
        // not do this, and would require a MSG_PEEK | MSG_TRUNC before each receive, and all kinds of races. Probably need to
        // force truncation and just make everybody pick a suitable receive buffer size.
        debug_assert!(self.buffer.is_empty());
        if let Some(remainder) = self.recv_buffer_len.checked_sub(self.buffer.len()) {
            self.buffer.reserve(remainder);
        }
        debug_assert!(self.buffer.capacity() >= self.recv_buffer_len);
        unsafe { self.buffer.set_len(self.recv_buffer_len); }
        let receiver = await!(self.inner.recv_with_resources(CHANNEL_MSG_MAX_RESOURCES, &mut self.buffer))?;
        debug_assert!(receiver.data_len() <= self.buffer.capacity());
        unsafe { self.buffer.set_len(receiver.data_len()); }
        let receiver = DeContext(RefCell::new(receiver));
        let buffer = &self.buffer;
        let result = ResourceDeserializeContext::set(&receiver, || {
            bincode::deserialize::<M>(buffer)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, format!("serialization failed: {}", err)))
        })?;
        self.buffer.clear();
        Ok(result)
    }
}

struct SerContext<'a>(RefCell<ChannelResourceSender<'a>>);

impl<'a> ResourceSerializeContext for SerContext<'a> {
    fn transmit_owned(&self, resource: Resource) -> io::Result<ResourceMetadata> {
        self.0.borrow_mut().move_resource(resource)
    }

    unsafe fn transmit_ref(&self, resource: ResourceRef) -> io::Result<ResourceMetadata> {
        self.0.borrow_mut().copy_resource(mem::transmute::<ResourceRef, ResourceRef<'a>>(resource))
    }
}

struct DeContext<'a>(RefCell<ChannelResourceReceiver<'a>>);

impl<'a> ResourceDeserializeContext for DeContext<'a> {
    fn recv(&self, metadata: ResourceMetadata) -> io::Result<Resource> {
        self.0.borrow_mut().recv_resource(metadata)
    }
}