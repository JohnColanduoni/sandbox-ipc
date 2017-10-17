use super::SendableFile;

extern crate tokio_uds;
extern crate libc;

mod channel;
mod sharedmem;

pub use self::channel::*;
pub use self::sharedmem::*;

use std::{io, fs, mem, ptr};
use std::cell::RefCell;
use std::borrow::Borrow;
use std::os::unix::prelude::*;

use serde::{Serialize, Deserialize, Serializer, Deserializer};

thread_local! {
    static CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS: RefCell<Option<HandleSender>> = RefCell::new(None);
    static CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS: RefCell<Option<HandleReceiver>> = RefCell::new(None);
}

#[derive(Debug)]
pub struct SendableFd<B = libc::c_int>(pub B);

impl Serialize for SendableFd {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        use serde::ser::Error;

        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            let sender = sender_guard.as_mut()
                .ok_or_else(|| S::Error::custom("attempted to serialize file descriptor outside of IPC channel"))?;
            unimplemented!()
        })
    }
}

impl<'a, B> Serialize for SendableFd<&'a B> where
    B: AsRawFd,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        SendableFd(self.0.as_raw_fd()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SendableFd {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        use serde::de::Error;

        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|receiver_guard| {
            let mut receiver_guard = receiver_guard.borrow_mut();
            let _receiver = receiver_guard.as_mut()
                .ok_or_else(|| D::Error::custom("attempted to deserialize handle outside of IPC channel"))?;
            // TODO: check fd
            unimplemented!()
        })
    }
}

impl<B> Serialize for SendableFile<B> where
    B: Borrow<fs::File>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        SendableFd(self.0.borrow().as_raw_fd()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SendableFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        let handle = SendableFd::deserialize(deserializer)?;
        Ok(SendableFile(unsafe { fs::File::from_raw_fd(handle.0) }))
    }
}

pub(crate) fn push_current_channel_serialize(channel: &Channel) -> impl Drop {
    struct Guard(Option<HandleSender>);

    impl Drop for Guard {
        fn drop(&mut self) {
            CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
                let mut sender_guard = sender_guard.borrow_mut();
                sender_guard.take().unwrap().commit();
                *sender_guard = self.0.take()
            });
        }
    }

    let mut sender = Some(HandleSender {
    });

    CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| 
        mem::swap(
            &mut *x.borrow_mut(),
            &mut sender,
        )
    );
    Guard(sender)
}

pub(crate) fn push_current_channel_deserialize(_channel: &Channel) -> impl Drop {
    struct Guard(Option<HandleReceiver>);

    impl Drop for Guard {
        fn drop(&mut self) {
            CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| *x.borrow_mut() = self.0.take());
        }
    }

    let mut receiver = Some(HandleReceiver);
    CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x|
        mem::swap(
            &mut *x.borrow_mut(),
            &mut receiver,
        )
    );
    Guard(receiver)
}


pub(crate) trait Channel {
}

struct HandleSender {
}

struct HandleReceiver;

impl HandleSender {
    fn commit(mut self) {
        //unimplemented!()
    }
}
