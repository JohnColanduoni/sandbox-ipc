use ::io::{SendableFile, SendableSocket};
use ::ser::{SerializeWrapper, SerializeWrapperGuard};

extern crate mio_named_pipes;
extern crate winapi;
extern crate kernel32;
extern crate advapi32;
extern crate ws2_32;

mod channel;
mod sharedmem;
mod sync;

pub use self::channel::*;
pub(crate) use self::sharedmem::*;
pub(crate) use self::sync::*;

use std::{io, fs, mem, ptr, fmt};
use std::cell::RefCell;
use std::borrow::Borrow;
use std::sync::{Once, ONCE_INIT};
use std::net::UdpSocket;
use std::os::windows::prelude::*;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use self::winapi::*;
use self::kernel32::*;
use self::ws2_32::*;
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

impl<'a, T> Serialize for SendableSocket<T> where
    T: AsRawSocket,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer,
    {
        use serde::ser::Error;

        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            let sender = sender_guard.as_mut()
                .ok_or_else(|| S::Error::custom("attempted to serialize handle outside of IPC channel"))?;
            let proto_info = unsafe {
                let mut proto_info: WsaProtoInfo = mem::zeroed();
                if WSADuplicateSocketW(
                    self.0.as_raw_socket(),
                    sender.remote_process_id().map_err(|x| S::Error::custom(x))?,
                    &mut proto_info as *mut WsaProtoInfo as _,
                ) != 0 {
                    return Err(S::Error::custom(io::Error::from_raw_os_error(WSAGetLastError())));
                }
                proto_info
            };
            proto_info.serialize(serializer)
        })
    }
}

static CALLED_WSASTARTUP: Once = ONCE_INIT;

impl<'de, T> Deserialize<'de> for SendableSocket<T> where
    T: FromRawSocket,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>,
    {
        use serde::de::Error;

        // Make sure we've called WSAStartup
        CALLED_WSASTARTUP.call_once(|| {
            // Any libstd socket operation will call WSAStartup
            let _ = UdpSocket::bind("127.0.0.1:9999");
        });

        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|receiver_guard| {
            let mut receiver_guard = receiver_guard.borrow_mut();
            let _receiver = receiver_guard.as_mut()
                .ok_or_else(|| D::Error::custom("attempted to deserialize handle outside of IPC channel"))?;
            let mut proto_info: WsaProtoInfo = WsaProtoInfo::deserialize(deserializer)?;
            let raw_socket = unsafe { WSASocketW(
                proto_info.iAddressFamily,
                proto_info.iSocketType,
                proto_info.iProtocol,
                &mut proto_info as *mut WsaProtoInfo as _,
                0,
                0, // Flags are ignored when receiving sockets from another process
            )};
            if raw_socket == INVALID_SOCKET {
                return Err(D::Error::custom(io::Error::from_raw_os_error(unsafe { WSAGetLastError() })));
            }
            Ok(SendableSocket(unsafe { T::from_raw_socket(raw_socket) }))
        })
    }
}

#[allow(bad_style)]
#[repr(C)]
#[derive(Serialize, Deserialize)]
struct WsaProtoInfo {
    dwServiceFlags1: DWORD,
    dwServiceFlags2: DWORD,
    dwServiceFlags3: DWORD,
    dwServiceFlags4: DWORD,
    dwProviderFlags: DWORD,
    #[serde(with = "serde_guid")]
    ProviderId: GUID,
    dwCatalogEntryId: DWORD,
    ProtocolChain: WsaProtocolChain,
    iVersion: c_int,
    iAddressFamily: c_int,
    iMaxSockAddr: c_int,
    iMinSockAddr: c_int,
    iSocketType: c_int,
    iProtocol: c_int,
    iProtocolMaxOffset: c_int,
    iNetworkByteOrder: c_int,
    iSecurityScheme: c_int,
    dwMessageSize: DWORD,
    dwProviderReserved: DWORD,
    #[serde(with = "serde_wchar256")]
    szProtocol: [WCHAR; 256],
}

#[allow(bad_style)]
#[repr(C)]
#[derive(Serialize, Deserialize, Debug)]
struct WsaProtocolChain {
    ChainLen: c_int,
    ChainEntries: [DWORD; 7],
}

mod serde_guid {
    use super::*;

    pub fn serialize<S>(f: &GUID, serializer: S) -> Result<S::Ok, S::Error> where
        S: Serializer,
    {
        (f.Data1, f.Data2, f.Data3, f.Data4).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<GUID, D::Error> where
        D: Deserializer<'de>,
    {
        let data: (c_ulong, c_ushort, c_ushort, [c_uchar; 8]) = Deserialize::deserialize(deserializer)?;
        Ok(GUID {
            Data1: data.0,
            Data2: data.1,
            Data3: data.2,
            Data4: data.3,
        })
    }
}

mod serde_wchar256 {
    use super::*;

    pub fn serialize<S>(f: &[WCHAR; 256], serializer: S) -> Result<S::Ok, S::Error> where
        S: Serializer,
    {
        use serde::ser::{SerializeTuple};

        let mut serializer = serializer.serialize_tuple(256)?;
        for wchar in f.iter() {
            serializer.serialize_element::<WCHAR>(wchar)?;
        }
        serializer.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[WCHAR; 256], D::Error> where
        D: Deserializer<'de>,
    {
        use serde::de::{Visitor, SeqAccess, Error};

        struct AVisitor;

        impl<'de> Visitor<'de> for AVisitor {
            type Value = [WCHAR; 256];

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("WCHAR string of length 256")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<[WCHAR; 256], V::Error>
                where V: SeqAccess<'de>
            {
                let mut buffer: [WCHAR; 256] = [0; 256];
                for (i, c) in buffer.iter_mut().enumerate() {
                    let read_c: WCHAR = seq.next_element()?
                              .ok_or_else(|| Error::invalid_length(i, &self))?;
                    *c = read_c;
                }
                Ok(buffer)
            }
        }

        deserializer.deserialize_tuple(256, AVisitor)
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
            target: unsafe { mem::transmute::<HandleTarget<&ProcessHandle>, HandleTarget<&'static ProcessHandle>>(channel.handle_target()) },
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
    fn handle_target(&self) -> HandleTarget<&ProcessHandle>;
}

struct HandleSender {
    target: HandleTarget<&'static ProcessHandle>,
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
                        process_handle.handle.0.get(), handle,
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
            HandleTarget::RemoteProcess(remote) => remote.handle.0.get(),
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

    fn remote_process_id(&self) -> io::Result<DWORD> {
        Ok(match self.target {
            HandleTarget::CurrentProcess => unsafe { GetCurrentProcessId() },
            HandleTarget::RemoteProcess(remote) => remote.id,
            HandleTarget::None => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "this pipe is not configured to send handles")),
        })
    }
}
