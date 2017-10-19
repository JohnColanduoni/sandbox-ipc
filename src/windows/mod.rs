use ::SendableFile;
use ::ser::{SerializeWrapper, SerializeWrapperGuard};

pub extern crate mio_named_pipes;
extern crate winapi;
extern crate kernel32;
extern crate advapi32;

mod channel;
mod sharedmem;
mod sync;

pub use self::channel::*;
pub use self::sharedmem::*;
pub(crate) use self::sync::*;

use std::{io, fs, mem, ptr};
use std::cell::RefCell;
use std::borrow::Borrow;
use std::os::windows::prelude::*;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use self::winapi::*;
use self::kernel32::*;
use winhandle::*;

thread_local! {
    static CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS: RefCell<Option<HandleSender>> = RefCell::new(None);
    static CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS: RefCell<Option<HandleReceiver>> = RefCell::new(None);
}

#[derive(Debug)]
pub struct SendableWinHandle<B = WinHandle>(pub B);

impl From<WinHandle> for SendableWinHandle {
    fn from(h: WinHandle) -> Self { SendableWinHandle(h) }
}

impl<'a, B> Serialize for SendableWinHandle<&'a B> where
    B: AsRawHandle,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        use serde::ser::Error;

        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            let sender = sender_guard.as_mut()
                .ok_or_else(|| S::Error::custom("attempted to serialize handle outside of IPC channel"))?;
            let remote_handle = sender.send(self.0.borrow().as_raw_handle())
                .map_err(|x| S::Error::custom(x))?;
            (remote_handle as usize).serialize(serializer)
        })
    }
}

impl Serialize for SendableWinHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        SendableWinHandle(&self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SendableWinHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        use serde::de::Error;

        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|receiver_guard| {
            let mut receiver_guard = receiver_guard.borrow_mut();
            let _receiver = receiver_guard.as_mut()
                .ok_or_else(|| D::Error::custom("attempted to deserialize handle outside of IPC channel"))?;
            let handle = usize::deserialize(deserializer)? as HANDLE;
            let handle = WinHandle::from_raw(handle)
                .ok_or_else(|| D::Error::custom("invalid Windows handle received"))?;
            Ok(SendableWinHandle(handle))
        })
    }
}

impl<B> Serialize for SendableFile<B> where
    B: Borrow<fs::File>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        SendableWinHandle(self.0.borrow()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SendableFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        let handle = SendableWinHandle::deserialize(deserializer)?;
        Ok(SendableFile(unsafe { fs::File::from_raw_handle(handle.0.into_raw()) }))
    }
}

pub(crate) struct ChannelSerializeWrapper;

impl<'a, C> SerializeWrapper<'a, C> for ChannelSerializeWrapper where
    C: Channel,
{
    type SerializeGuard = ChannelSerializeGuard;
    type DeserializeGuard = ChannelDeserializeGuard;

    fn before_serialize(channel: &'a mut C) -> ChannelSerializeGuard {
        let mut sender = Some(HandleSender {
            target: channel.handle_target(),
            handles: Vec::new(),
        });

        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| 
            mem::swap(
                &mut *x.borrow_mut(),
                &mut sender,
            )
        );
        ChannelSerializeGuard(sender)
    }

    fn before_deserialize(_channel: &'a mut C) -> ChannelDeserializeGuard {
        let mut receiver = Some(HandleReceiver);
        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x|
            mem::swap(
                &mut *x.borrow_mut(),
                &mut receiver,
            )
        );
        ChannelDeserializeGuard(receiver)
    }
}

pub(crate) struct ChannelSerializeGuard(Option<HandleSender>);

impl Drop for ChannelSerializeGuard {
    fn drop(&mut self) {
        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            *sender_guard = self.0.take()
        });
    }
}

impl<'a> SerializeWrapperGuard<'a> for ChannelSerializeGuard {
    fn commit(self) {
        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            sender_guard.as_mut().unwrap().handles.clear();
        });
    }
}

pub(crate) struct ChannelDeserializeGuard(Option<HandleReceiver>);

impl Drop for ChannelDeserializeGuard {
    fn drop(&mut self) {
        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| *x.borrow_mut() = self.0.take());
    }
}

impl<'a> SerializeWrapperGuard<'a> for ChannelDeserializeGuard {
    fn commit(self) {
    }
}

pub(crate) trait Channel {
    fn handle_target(&self) -> HandleTarget<HANDLE>;
}

struct HandleSender {
    target: HandleTarget<HANDLE>,
    handles: Vec<HANDLE>,
}

impl Drop for HandleSender {
    fn drop(&mut self) {
        // If this is called without self.handles being empty, there was a failure somewhere and
        // we should close all the handles we sent.
        match self.target {
            HandleTarget::CurrentProcess => {
                for &handle in self.handles.iter() {
                    unsafe { CloseHandle(handle); }
                }
            },
            HandleTarget::RemoteProcess(process_handle) => {
                for &handle in self.handles.iter() {
                    if unsafe { DuplicateHandle(
                        process_handle, handle,
                        ptr::null_mut(), ptr::null_mut(),
                        0, FALSE, DUPLICATE_CLOSE_SOURCE,
                    ) } == FALSE {
                        error!("Closing remote handle via DuplicateHandle failed: {}", io::Error::last_os_error());
                    }
                }
            },
            HandleTarget::None => {},
        }
    }
}

struct HandleReceiver;

impl HandleSender {
    fn send(&mut self, handle: HANDLE) -> io::Result<HANDLE> {
        let remote_process_handle = match self.target {
            HandleTarget::CurrentProcess => unsafe { GetCurrentProcess() },
            HandleTarget::RemoteProcess(remote) => remote,
            HandleTarget::None => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "this pipe is not configured to send handles")),
        };

        unsafe {
            let mut remote_handle = ptr::null_mut();
            winapi_bool_call!(log: DuplicateHandle(
                GetCurrentProcess(), handle,
                remote_process_handle, &mut remote_handle,
                0, FALSE, DUPLICATE_SAME_ACCESS,
            ))?;

            self.handles.push(remote_handle);

            Ok(remote_handle)
        }
    }
}
