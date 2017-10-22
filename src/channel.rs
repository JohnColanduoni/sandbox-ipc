use super::*;
use ser::BincodeDatagram;

use std::{io, process};
use std::marker::PhantomData;
use std::time::Duration;
use std::ffi::OsStr;

use serde::{Serialize, Deserialize};
use futures::{Stream, Sink, Poll, StartSend};
use tokio::reactor::{Handle as TokioHandle};
use tokio::{AsyncRead, AsyncWrite};

pub struct MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    inner: BincodeDatagram<platform::MessageChannel, T, R, platform::ChannelSerializeWrapper>,
    max_message_size: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChildMessageChannel {
    pub(crate) inner: platform::ChildMessageChannel,
    max_message_size: usize,
}

impl<T, R> MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    pub fn pair(tokio_handle: &TokioHandle, max_message_size: usize) -> io::Result<(Self, Self)> {
        let (a, b) = platform::MessageChannel::pair(tokio_handle)?;

        Ok((
            MessageChannel { inner: BincodeDatagram::wrap(a, max_message_size), max_message_size },
            MessageChannel { inner: BincodeDatagram::wrap(b, max_message_size), max_message_size },
        ))
    }

    pub fn from_raw(channel: RawMessageChannel, max_message_size: usize) -> io::Result<Self> {
        Ok(MessageChannel { inner: BincodeDatagram::wrap(channel.inner, max_message_size), max_message_size })
    }

    pub fn send_to_child<F>(self, command: &mut process::Command, transmit_and_launch: F) -> io::Result<process::Child> where
        F: FnOnce(&mut process::Command, &ChildMessageChannel) -> io::Result<process::Child>
    {
        let inner = self.inner.into_inner();
        let max_message_size = self.max_message_size;
        inner.send_to_child(command, |command, channel| {
            let channel = ChildMessageChannel { inner: channel, max_message_size };
            transmit_and_launch(command, &channel)
        })
    }
}

impl ChildMessageChannel {
    pub fn into_channel<T, R>(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel<T, R>> where
        T: Serialize,
        R: for<'de> Deserialize<'de>
    {
        let inner = self.inner.into_channel(tokio_loop)?;

        Ok(MessageChannel {
            inner: BincodeDatagram::wrap(inner, self.max_message_size),
            max_message_size: self.max_message_size,
        })
    }
}

impl<T, R> Stream for MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>,
{
    type Item = R;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<R>, io::Error> {
        self.inner.poll()
    }
}

impl<T, R> Sink for MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>,
{
    type SinkItem = T;
    type SinkError = io::Error;

    fn start_send(&mut self, item: T) -> StartSend<T, io::Error> {
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.inner.poll_complete()
    }
}

pub struct NamedMessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    inner: platform::NamedMessageChannel,
    max_message_size: usize,
    _phantom: PhantomData<(T, R)>,
}

impl<T, R> NamedMessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    pub fn new(tokio_loop: &TokioHandle, max_message_size: usize) -> io::Result<Self> {
        let inner = platform::NamedMessageChannel::new(tokio_loop)?;
        Ok(NamedMessageChannel { inner, max_message_size, _phantom: PhantomData })
    }

    pub fn name(&self) -> &OsStr { self.inner.name() }

    pub fn accept(self, timeout: Option<Duration>) -> io::Result<MessageChannel<T, R>> {
        let inner = self.inner.accept(timeout)?;
        Ok(MessageChannel {
            inner: BincodeDatagram::wrap(inner, self.max_message_size),
            max_message_size: self.max_message_size,
        })
    }

    pub fn connect<N>(name: N, timeout: Option<Duration>, tokio_loop: &TokioHandle, max_message_size: usize) -> io::Result<MessageChannel<T, R>> where
        N: AsRef<OsStr>,
    {
        let inner = platform::NamedMessageChannel::connect(name.as_ref(), timeout, tokio_loop)?;
        Ok(MessageChannel {
            inner: BincodeDatagram::wrap(inner, max_message_size),
            max_message_size,
        })
    }
}

pub struct RawMessageChannel {
    inner: platform::MessageChannel,
}

impl RawMessageChannel {
    pub fn pair(tokio_loop: &TokioHandle) -> io::Result<(Self, Self)> {
        let (a, b) = platform::MessageChannel::pair(tokio_loop)?;

        Ok((
            RawMessageChannel { inner: a },
            RawMessageChannel { inner: b },
        ))
    }

    pub fn send_to_child<F>(self, command: &mut process::Command, transmit_and_launch: F) -> io::Result<process::Child> where
        F: FnOnce(&mut process::Command, ChildRawMessageChannel) -> io::Result<process::Child>
    {
        self.inner.send_to_child(command, |command, child_channel| {
            transmit_and_launch(command, ChildRawMessageChannel(child_channel))
        })
    }

    pub fn send_to_child_custom<F, T>(self, transmit_and_launch: F) -> io::Result<T> where
        F: FnOnce(ChildRawMessageChannel) -> io::Result<(ProcessToken, T)>
    {
        self.inner.send_to_child_custom(|child_channel| {
            let (token, t) = transmit_and_launch(ChildRawMessageChannel(child_channel))?;
            Ok((token.0, t))
        })
    }
}

impl io::Read for RawMessageChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}

impl AsyncRead for RawMessageChannel {
}

impl io::Write for RawMessageChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl AsyncWrite for RawMessageChannel {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.inner.shutdown()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChildRawMessageChannel(pub(crate) platform::ChildMessageChannel);

impl ChildRawMessageChannel {
    pub fn into_channel(self, tokio_loop: &TokioHandle) -> io::Result<RawMessageChannel> {
        Ok(RawMessageChannel {
            inner: self.0.into_channel(tokio_loop)?,
        })
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PreRawMessageChannel(platform::PreMessageChannel);

impl PreRawMessageChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = platform::PreMessageChannel::pair()?;

        Ok((
            PreRawMessageChannel(a),
            PreRawMessageChannel(b),
        ))
    }

    pub fn into_channel(self, remote_process: ProcessToken, tokio_loop: &TokioHandle) -> io::Result<RawMessageChannel> {
        let channel = self.0.into_channel(remote_process.0, tokio_loop)?;
        Ok(RawMessageChannel { inner: channel })
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessToken(pub(crate) platform::ProcessToken);

impl ProcessToken {
    pub fn current() -> io::Result<Self> {
        Ok(ProcessToken(
            platform::ProcessToken::current()?
        ))
    }

    pub fn clone(&self) -> io::Result<Self> {
        Ok(ProcessToken(
            self.0.clone()?
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::check_send;

    #[test]
    fn pre_raw_message_channel_is_send() {
        let (a, _b) = PreRawMessageChannel::pair().unwrap();
        check_send(&a);
    }

    #[test]
    fn process_token_is_send() {
        let token = ProcessToken::current().unwrap();
        check_send(&token);
    }
}
