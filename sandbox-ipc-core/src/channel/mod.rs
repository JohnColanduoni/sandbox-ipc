pub mod raw;

use crate::platform;
use crate::resource::ResourceTransceiver;

use std::{io};
use std::marker::PhantomData;

use compio_core::queue::Registrar;

pub struct Channel<T, R> {
    inner: platform::Channel,
    _phantom: PhantomData<(T, R)>,
}

impl<T, R> Channel<T, R> {

}

pub struct PreChannel<T, R> {
    inner: platform::PreChannel,
    _phantom: PhantomData<(T, R)>,
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
            _phantom: PhantomData,
        })
    }

    pub fn into_resource_channel(self, queue: &Registrar, resource_tranceiver: ResourceTransceiver) -> io::Result<Channel<T, R>> {
        Ok(Channel {
            inner: self.inner.into_resource_channel(queue, resource_tranceiver.inner)?,
            _phantom: PhantomData,
        })
    }
}