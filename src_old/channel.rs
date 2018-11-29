use super::*;
use ser::BincodeDatagram;

use std::{io, process};
use std::marker::PhantomData;
use std::time::Duration;
use std::ffi::OsStr;

use serde::{Serialize, Deserialize};
use futures::{Stream, Sink, Poll, StartSend};
use tokio::reactor::{Handle as TokioHandle};
use tokio::io::{AsyncRead, AsyncWrite};

/// A basic channel for sending serializable types between processes.
/// 
/// `MessageChannel`s are the fundamental IPC primitive in the crate. Most other primitives must be sent across a `MessageChannel` 
/// before they can be used to communicate. They are initially best established by between parent and child processes 
/// via `MessageChannel::establish_with_child`, but they can also be connected by name via `NamedMessageChannel`. Once initial 
/// channels are established, further channels can be set up by creating `PreMessageChannel` pairs and sending them to their final destinations.
/// 
/// `MessageChannel`s are capable of sending various OS objects, such as file descriptors on Unix systems and handles on Windows. To do so either
/// use platforms specific structures such as `SendableFd` or `SendableWinHandle`, or cross-platform wrappers like `SendableFile`. Note that though
/// these types implement `Serialize` and `Deserialize`, attempts to `serialize` or `deserialize` them outside of the send/receive mechanisms of a suitable IPC object
/// will fail.
pub struct MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    pub(crate) inner: BincodeDatagram<platform::MessageChannel, T, R, platform::ChannelSerializeWrapper>,
    #[allow(unused)]
    max_message_size: usize,
}

/// A serializable type for establishing `MessageChannel`s with child processes.
/// 
/// This type should be serialized into some format suitable for inclusion in arguments or
/// environment variables that can be passed to a child process at creation.
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

    pub fn establish_with_child<F>(command: &mut process::Command, max_message_size: usize, tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, process::Child)> where
        F: FnOnce(&mut process::Command, &ChildMessageChannel) -> io::Result<process::Child>
    {
        let (raw_channel, child) = platform::MessageChannel::establish_with_child(command, tokio_loop, |command, channel| {
            let channel = ChildMessageChannel { inner: channel, max_message_size };
            transmit_and_launch(command, &channel)
        })?;

        Ok((MessageChannel { inner: BincodeDatagram::wrap(raw_channel, max_message_size), max_message_size }, child))
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

/// A channel which can be established by sending an automatically generated unique
/// name to another process.
/// 
/// # Security
/// 
/// It depends by platforms, but generally at the very least any other process running under
/// the same user will be able to open this channel instead of the desired process. This method
/// is also not suitable for cooperative processes running with diminished permissions. It is best
/// to send channels to child processes at creation, and to negotiate any further channels over
/// those initial ones. The two sides of a `PreMessageChannel` can be sent freely over a network
/// of parent-child channels if need be.
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

/// A sendable handle for establishing `MessageChannel`s with processes you can already communicate with.
///
/// `PreMessageChannel`s are created in pairs, and then can be freely sent along any IPC method in this
/// crate capable of transmitting OS resources. When they reach their destination they can be converted
/// into `MessageChannel`s.
#[derive(Serialize, Deserialize, Debug)]
pub struct PreMessageChannel<T, R> where
    T: Serialize,
    R: for<'des> Deserialize<'des>
{
    pub(crate) inner: platform::PreMessageChannel,
    max_message_size: usize,
    _phantom: PhantomData<(T, R)>,
}

impl<T, R> PreMessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    pub fn pair(max_message_size: usize) -> io::Result<(Self, Self)> {
        let (a, b) = platform::PreMessageChannel::pair()?;

        Ok((
            PreMessageChannel { inner: a, max_message_size, _phantom: PhantomData },
            PreMessageChannel { inner: b, max_message_size, _phantom: PhantomData },
        ))
    }

    /// Creates a `MessageChannel` from a transmitted pre-channel and a remote process handle.
    /// 
    /// # Security
    /// 
    /// You should only allow more trusted processes to hold unsealed channels to less trusted processes. On some platforms unsealed channels
    /// allow the holder to interfere with the other process.
    /// 
    /// Using an incorrect remote process handle may cause this function to fail or transmission of OS resources (files, sockets, etc.)
    /// accross the channel to fail.
    pub fn into_channel(self, remote_process: ProcessHandle, tokio_loop: &TokioHandle) -> io::Result<MessageChannel<T, R>> {
        let channel = self.inner.into_channel(remote_process.0, tokio_loop)?;
        Ok(MessageChannel { inner: BincodeDatagram::wrap(channel, self.max_message_size), max_message_size: self.max_message_size })
    }

    /// Creates a sealed `MessageChannel` from a transmitted pre-channel.
    /// 
    /// Sealed channels cannot transmit OS resources (files, sockets, etc.) but can still transmit data (they may receive OS resources if the other side is unsealed).
    /// It is recommended that you use a sealed channel handle when communicating from the less trusted side of a channel.
    pub fn into_sealed_channel(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel<T, R>> {
        let channel = self.inner.into_sealed_channel(tokio_loop)?;
        Ok(MessageChannel { inner: BincodeDatagram::wrap(channel, self.max_message_size), max_message_size: self.max_message_size })
    }
}

/// A channel which raw binary messages can be send over.
/// 
/// This can be upgraded into a `MessageChannel` at any time.
pub struct RawMessageChannel {
    pub(crate) inner: platform::MessageChannel,
}

impl RawMessageChannel {
    pub fn pair(tokio_loop: &TokioHandle) -> io::Result<(Self, Self)> {
        let (a, b) = platform::MessageChannel::pair(tokio_loop)?;

        Ok((
            RawMessageChannel { inner: a },
            RawMessageChannel { inner: b },
        ))
    }

    pub fn establish_with_child<F>(command: &mut process::Command, tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, process::Child)> where
        F: FnOnce(&mut process::Command, ChildRawMessageChannel) -> io::Result<process::Child>
    {
        let (channel, child) = platform::MessageChannel::establish_with_child(command, tokio_loop, |command, child_channel| {
            transmit_and_launch(command, ChildRawMessageChannel(child_channel))
        })?;

        Ok((RawMessageChannel { inner: channel }, child))
    }

    pub fn establish_with_child_custom<F, T>(tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, T)> where
        F: FnOnce(ChildRawMessageChannel) -> io::Result<(ProcessHandle, T)>
    {
        let (channel, t) = platform::MessageChannel::establish_with_child_custom(tokio_loop, |child_channel| {
            let (token, t) = transmit_and_launch(ChildRawMessageChannel(child_channel))?;
            Ok((token.0, t))
        })?;

        Ok((RawMessageChannel { inner: channel }, t))
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

/// A serializable type for establishing `RawMessageChannel`s with child processes.
/// 
/// This type should be serialized into some format suitable for inclusion in arguments or
/// environment variables that can be passed to a child process at creation.
#[derive(Serialize, Deserialize, Debug)]
pub struct ChildRawMessageChannel(pub(crate) platform::ChildMessageChannel);

impl ChildRawMessageChannel {
    pub fn into_channel(self, tokio_loop: &TokioHandle) -> io::Result<RawMessageChannel> {
        Ok(RawMessageChannel {
            inner: self.0.into_channel(tokio_loop)?,
        })
    }
}

/// A sendable handle for establishing `RawMessageChannel`s with processes you can already communicate with.
///
/// `PreRawMessageChannel`s are created in pairs, and then can be freely sent along any IPC method in this
/// crate capable of transmitting OS resources. When they reach their destination they can be converted
/// into `RawMessageChannel`s.
#[derive(Serialize, Deserialize, Debug)]
pub struct PreRawMessageChannel(pub(crate) platform::PreMessageChannel);

impl PreRawMessageChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = platform::PreMessageChannel::pair()?;

        Ok((
            PreRawMessageChannel(a),
            PreRawMessageChannel(b),
        ))
    }

    /// Creates a `RawMessageChannel` from a transmitted pre-channel and a remote process handle.
    /// 
    /// # Security
    /// 
    /// You should only allow more trusted processes to hold unsealed channels to less trusted processes. On some platforms unsealed channels
    /// allow the holder to interfere with the other process.
    /// 
    /// Using an incorrect remote process handle may cause this function to fail or transmission of OS resources (files, sockets, etc.)
    /// accross the channel to fail.
    pub fn into_raw_channel(self, remote_process: ProcessHandle, tokio_loop: &TokioHandle) -> io::Result<RawMessageChannel> {
        let channel = self.0.into_channel(remote_process.0, tokio_loop)?;
        Ok(RawMessageChannel { inner: channel })
    }

    /// Creates a sealed `RawMessageChannel` from a transmitted pre-channel.
    /// 
    /// Sealed channels cannot transmit OS resources (files, sockets, etc.) but can still transmit data (they may receive OS resources if the other side is unsealed).
    /// It is recommended that you use a sealed channel handle when communicating from the less trusted side of a channel.
    pub fn into_sealed_raw_channel(self, tokio_loop: &TokioHandle) -> io::Result<RawMessageChannel> {
        let channel = self.0.into_sealed_channel(tokio_loop)?;
        Ok(RawMessageChannel { inner: channel })
    }
}

/// `ProcessHandle`s are needed to establish unsealed channels (i.e. channels that can transmit OS resources).
/// 
/// # Security
/// 
/// You should not allow a less trusted process to either hold the `ProcessHandle` of or an unsealed channel to a more trusted process unless
/// absolutely necessary. On some platforms `ProcessHandle`s and unsealed channels allow the holder to interfere with the other process.
#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessHandle(pub(crate) platform::ProcessHandle);

impl ProcessHandle {
    /// Gets the `ProcessHandle` for the current process.
    pub fn current() -> io::Result<Self> {
        Ok(ProcessHandle(
            platform::ProcessHandle::current()?
        ))
    }

    pub fn from_child(child: &process::Child) -> io::Result<Self> {
        Ok(ProcessHandle(
            platform::ProcessHandle::from_child(child)?
        ))
    }

    /// Creates a copy of this `ProcessHandle`.
    pub fn clone(&self) -> io::Result<Self> {
        Ok(ProcessHandle(
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
    fn process_handle_is_send() {
        let token = ProcessHandle::current().unwrap();
        check_send(&token);
    }
}
