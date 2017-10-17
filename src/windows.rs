use super::*;

pub extern crate mio_named_pipes;
extern crate winapi;
extern crate kernel32;
extern crate advapi32;

use std::{io, fs, mem, ptr, process};
use std::collections::Bound;
use std::collections::range::RangeArgument;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::borrow::Borrow;
use std::os::windows::prelude::*;
use std::ffi::{OsString};

use uuid::Uuid;
use serde::{Serialize, Deserialize, Serializer, Deserializer};
use tokio::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
use futures::{Poll, Async};
use self::mio_named_pipes::NamedPipe as MioNamedPipe;
use self::winapi::*;
use self::kernel32::*;
use winhandle::*;

const SECURITY_IDENTIFICATION: DWORD = 65536;

#[derive(Debug)]
pub struct MessageChannel {
    pipe: NamedPipe,
    server: bool,
    target_state: Arc<Mutex<HandleTargetState>>,
}

// On Windows, adding a kernel object to an IOCP (which Tokio does) causes it to break when sent to another process.
// To deal with this, we need to delay actually adding the named pipe to the event loop so it can be successfully sent as
// long as reading/writing was never attempted on it. This also allows us to give a good error message if this requirement
// is broken.
#[derive(Debug)]
enum NamedPipe {
    Unregistered { pipe: WinHandle, tokio: TokioHandle },
    Registered { pipe: PollEvented<MioNamedPipe> }
}

#[derive(Debug)]
pub enum HandleTarget<H = WinHandle> {
    None,
    CurrentProcess,
    RemoteProcess(H),
}

#[derive(Debug)]
enum HandleTargetState {
    Unsent,
    ServerSentTo(WinHandle),
    ClientSentTo(WinHandle),
}

impl MessageChannel {
    pub fn pair(handle: &TokioHandle) -> io::Result<(MessageChannel, MessageChannel)> {
        let (server_pipe, client_pipe) = raw_pipe_pair(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS)?;
        let state = Arc::new(Mutex::new(HandleTargetState::Unsent));

        Ok((
            MessageChannel {
                pipe: NamedPipe::Unregistered { pipe: server_pipe, tokio: handle.clone() },
                server: true,
                target_state: state.clone(),
            },
            MessageChannel {
                pipe: NamedPipe::Unregistered { pipe: client_pipe, tokio: handle.clone() },
                server: false,
                target_state: state.clone(),
            },
        ))
    }

    pub fn send_to_child<F>(self, command: &mut process::Command, transmit_and_launch: F) -> io::Result<process::Child> where
        F: FnOnce(&mut process::Command, &ChildMessageChannel) -> io::Result<process::Child>
    {
        let mut target_state = self.target_state.lock().unwrap();

        let inheritable_process_handle = match *target_state {
            HandleTargetState::Unsent => WinHandle::from_raw(unsafe { GetCurrentProcess() }).unwrap().clone_ex(true, ClonedHandleAccess::Explicit(PROCESS_DUP_HANDLE))?,
            HandleTargetState::ClientSentTo(ref remote) => {
                assert!(self.server);
                unimplemented!()
            },
            HandleTargetState::ServerSentTo(ref remote) => {
                assert!(!self.server);
                unimplemented!()
            },
        };

        let inheritable_pipe = match self.pipe {
            NamedPipe::Unregistered { ref pipe, .. } => {
                pipe.clone_ex(true, ClonedHandleAccess::Same)?
            },
            NamedPipe::Registered { .. } => return Err(io::Error::new(io::ErrorKind::Other, "IO was already performed on this channel, preventing it from being sent to another process")),
        };

        let to_be_sent = ChildMessageChannel {
            channel_handle: inheritable_pipe,
            remote_process_handle: inheritable_process_handle,
            server: self.server,
        };

        let child = transmit_and_launch(command, &to_be_sent)?;

        if self.server {
            *target_state = HandleTargetState::ServerSentTo(WinHandle::cloned(&child)?);
        } else {
            *target_state = HandleTargetState::ClientSentTo(WinHandle::cloned(&child)?);
        }

        Ok(child)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChildMessageChannel {
    #[serde(with = "inheritable_channel_serialize")]
    channel_handle: WinHandle,
    #[serde(with = "inheritable_channel_serialize")]
    remote_process_handle: WinHandle,
    server: bool,
}

mod inheritable_channel_serialize {
    use super::*;

    pub fn serialize<S>(handle: &WinHandle, serializer: S) -> Result<S::Ok, S::Error> where
        S: Serializer,
    {
        usize::serialize(&(handle.get() as usize), serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<WinHandle, D::Error> where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let handle = usize::deserialize(deserializer)?;
        WinHandle::from_raw(handle as HANDLE)
            .ok_or_else(|| D::Error::custom(io::Error::new(io::ErrorKind::InvalidData, "received invalid inherited handle")))
    }
}

impl Channel for MessageChannel {
    fn handle_target(&self) -> HandleTarget<HANDLE> {
        match *self.target_state.lock().unwrap() {
            HandleTargetState::Unsent => HandleTarget::CurrentProcess,
            HandleTargetState::ServerSentTo(ref remote) => {
                assert!(!self.server);
                HandleTarget::RemoteProcess(remote.get())
            },
            HandleTargetState::ClientSentTo(ref remote) => {
                assert!(self.server);
                HandleTarget::RemoteProcess(remote.get())
            },
        }
    }
}

impl ChildMessageChannel {
    pub fn into_channel(self, tokio: &TokioHandle) -> io::Result<MessageChannel> {
        let state = Arc::new(Mutex::new({
            if self.server {
                HandleTargetState::ClientSentTo(self.remote_process_handle)
            } else {
                HandleTargetState::ServerSentTo(self.remote_process_handle)
            }
        }));

        Ok(
            MessageChannel {
                pipe: NamedPipe::Unregistered { pipe: self.channel_handle, tokio: tokio.clone() },
                server: self.server,
                target_state: state.clone(),
            }
        )
    }
}

#[derive(Debug)]
pub struct SharedMem {
    handle: WinHandle,
    size: usize,
}

pub struct SharedMemMap<T = SharedMem> where
    T: Borrow<SharedMem>
{
    mem: T,
    pointer: *mut u8,
    len: usize,
}

impl<T> Drop for SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    fn drop(&mut self) {
        unsafe {
            if UnmapViewOfFile(self.pointer as _) == FALSE {
                error!("UnmapViewOfFile failed: {}", io::Error::last_os_error());
            }
        }
    }
}

impl SharedMem {
    pub fn new(size: usize) -> io::Result<SharedMem> {
        unsafe {
            let handle = winapi_handle_call! { log: CreateFileMappingW(
                ptr::null_mut(),
                ptr::null_mut(),
                PAGE_READWRITE,
                (size >> 32) as DWORD,
                (size & 0xFFFFFFFF) as DWORD,
                ptr::null()
            )}?;

            Ok(SharedMem { handle, size })
        }
    }

    pub fn size(&self) -> usize { self.size }

    pub fn clone(&self, read_only: bool) -> io::Result<SharedMem> {
        let access = if read_only { FILE_MAP_READ } else { FILE_MAP_ALL_ACCESS };

        unsafe {
            let mut handle = WinHandleTarget::new();
            winapi_bool_call!(DuplicateHandle(
                GetCurrentProcess(), self.handle.get(),
                GetCurrentProcess(), &mut *handle,
                access, FALSE, 0
            ))?;

            Ok(SharedMem { handle: handle.unwrap(), size: self.size })
        }
    }

    pub fn map<R>(self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<Self>> where
        R: RangeArgument<usize>,
    {
        Self::map_with(self, range, access)
    }

    pub fn map_ref<R>(&self, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<&Self>> where
        R: RangeArgument<usize>,
    {
        Self::map_with(self, range, access)
    }

    pub fn map_with<T, R>(t: T, range: R, access: SharedMemAccess) -> io::Result<SharedMemMap<T>> where
        T: Borrow<SharedMem>,
        R: RangeArgument<usize>,
    {
        let access = match access {
            SharedMemAccess::Read => FILE_MAP_READ,
            SharedMemAccess::ReadWrite => FILE_MAP_WRITE,
        };

        let offset = match range.start() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => i + 1,
            Bound::Unbounded => 0,
        };
        let len = match range.start() {
            Bound::Included(i) => i + 1,
            Bound::Excluded(i) => *i,
            Bound::Unbounded => t.borrow().size(),
        };

        unsafe {
            let addr = MapViewOfFile(
                t.borrow().handle.get(),
                access,
                (offset >> 32) as DWORD,
                (offset & 0xFFFFFFFF) as DWORD,
                len as SIZE_T
            );
            if addr.is_null() {
                return Err(io::Error::last_os_error());
            }

            Ok(SharedMemMap {
                mem: t,
                pointer: addr as _,
                len
            })
        }
    }
}

impl<T> SharedMemMap<T> where
    T: Borrow<SharedMem>,
{
    pub fn unmap(self) -> io::Result<T> {
        unsafe {
            let pointer = self.pointer;
            // We have to do these gymnastics to prevent the destructor from running
            let memory = ptr::read(&self.mem);
            mem::forget(self);

            winapi_bool_call!(UnmapViewOfFile(
                pointer as _
            ))?;

            Ok(memory)
        }
    }

    pub unsafe fn pointer(&self) -> *mut u8 { self.pointer }
    pub fn len(&self) -> usize { self.len }
}

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

impl Serialize for SharedMem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        (SendableWinHandle(&self.handle), self.size).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SharedMem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        let (handle, size): (SendableWinHandle, usize) = Deserialize::deserialize(deserializer)?;
        Ok(SharedMem { handle: handle.0, size })
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
        target: channel.handle_target(),
        handles: Vec::new(),
    });

    CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| 
        mem::swap(
            &mut *x.borrow_mut(),
            &mut sender,
        )
    );
    Guard(sender)
}

pub(crate) fn push_current_channel_deserialize(channel: &Channel) -> impl Drop {
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

    fn commit(mut self) {
        self.handles.clear();
    }
}

fn raw_pipe_pair(pipe_type: DWORD) -> io::Result<(WinHandle, WinHandle)> {
    unsafe {
        let pipe_name = OsString::from(format!(r#"\\.\pipe\{}"#, Uuid::new_v4()));

        debug!("creating named pipe pair {:?}", pipe_name);
        let server_pipe = winapi_handle_call! { CreateNamedPipeW(
            WString::from(&pipe_name).unwrap().as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            pipe_type,
            1,
            0, 0,
            0,
            ptr::null_mut()
        )}?;

        // Begin connection operation on server half
        let mut overlapped: OVERLAPPED = mem::zeroed();

        if ConnectNamedPipe(
            server_pipe.get(),
            &mut overlapped
        ) != 0 {
            return Err(io::Error::last_os_error());
        }

        if GetLastError() != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }

        let mut security_attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: FALSE,
        };

        let client_pipe = winapi_handle_call! { CreateFileW(
            WString::from(&pipe_name).unwrap().as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &mut security_attributes,
            OPEN_EXISTING,
            SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
            ptr::null_mut()
        )}?;

        let mut bytes: DWORD = 0;
        winapi_bool_call!(GetOverlappedResultEx(
            server_pipe.get(),
            &mut overlapped,
            &mut bytes,
            1000,
            TRUE
        ))?;
    
        Ok((server_pipe, client_pipe))
    }
}

impl io::Write for MessageChannel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.pipe.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

impl AsyncWrite for MessageChannel {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.pipe.shutdown()
    }
}

impl io::Read for MessageChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.pipe.read(buffer)
    }
}

impl AsyncRead for MessageChannel {

}

impl NamedPipe {
    fn ensure_registered(&mut self) -> io::Result<&mut PollEvented<MioNamedPipe>> {
        let pipe = match *self {
            NamedPipe::Registered { ref mut pipe } => return Ok(pipe),
            NamedPipe::Unregistered { ref pipe, ref tokio } => {
                PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(pipe.get()) }, tokio)?
            },
        };

        unsafe {
            ptr::write(self, NamedPipe::Registered { pipe });
        }

        self.ensure_registered()
    }
}

impl io::Write for NamedPipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.ensure_registered()?.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ensure_registered()?.flush()
    }
}

impl AsyncWrite for NamedPipe {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.ensure_registered()?.get_ref().disconnect()?;

        Ok(Async::Ready(()))
    }
}

impl io::Read for NamedPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.ensure_registered()?.read(buffer)
    }
}

impl AsyncRead for NamedPipe {

}