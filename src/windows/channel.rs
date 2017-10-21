use super::Channel;

use std::{io, mem, ptr, process};
use std::time::Duration;
use std::sync::{Arc, Mutex};
use std::os::windows::prelude::*;
use std::ffi::{OsString, OsStr};

use uuid::Uuid;
use serde::{Serializer, Deserializer};
use tokio::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
use futures::{Poll, Async};
use platform::mio_named_pipes::NamedPipe as MioNamedPipe;
use platform::winapi::*;
use platform::kernel32::*;
use winhandle::*;

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
        F: FnOnce(&mut process::Command, ChildMessageChannel) -> io::Result<process::Child>
    {
        self.send_to_child_custom(|to_be_sent| {
            let child = transmit_and_launch(command, to_be_sent)?;
            Ok((WinHandle::cloned(&child)?, child))
        })
    }

    pub fn send_to_child_custom<F, T>(self, transmit_and_launch: F) -> io::Result<T> where
        F: FnOnce(ChildMessageChannel) -> io::Result<(WinHandle, T)>,
    {
        let mut target_state = self.target_state.lock().unwrap();

        let inheritable_process_handle = match *target_state {
            HandleTargetState::Unsent => WinHandle::from_raw(unsafe { GetCurrentProcess() }).unwrap().clone_ex(true, ClonedHandleAccess::Explicit(PROCESS_DUP_HANDLE))?,
            HandleTargetState::ClientSentTo(_) => {
                assert!(self.server);
                unimplemented!()
            },
            HandleTargetState::ServerSentTo(_) => {
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

        let (child_handle, t) = transmit_and_launch(to_be_sent)?;

        if self.server {
            *target_state = HandleTargetState::ServerSentTo(child_handle);
        } else {
            *target_state = HandleTargetState::ClientSentTo(child_handle);
        }

        Ok(t)
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

    use serde::{Serialize, Deserialize};

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

    pub fn handles(&self) -> ChildMessageChannelHandles {
        ChildMessageChannelHandles { channel_handle: self, index: 0 }
    }
}

pub struct ChildMessageChannelHandles<'a> {
    channel_handle: &'a ChildMessageChannel,
    index: usize,
}

impl<'a> Iterator for ChildMessageChannelHandles<'a> {
    type Item = HANDLE;

    fn next(&mut self) -> Option<HANDLE> {
        let handle = match self.index {
            0 => self.channel_handle.channel_handle.get(),
            1 => self.channel_handle.remote_process_handle.get(),
            _ => return None,
        };
        self.index += 1;
        Some(handle)
    }
}

pub struct NamedMessageChannel {
    name: OsString,
    server_pipe: WinHandle,
    overlapped: Box<OVERLAPPED>,
    tokio_loop: TokioHandle,
}

impl NamedMessageChannel {
    pub fn new(tokio: &TokioHandle) -> io::Result<Self> {
        let pipe_name = format!(r#"\\.\pipe\{}"#, Uuid::new_v4());

        unsafe {
            let server_pipe = winapi_handle_call! { CreateNamedPipeW(
                WString::from(&pipe_name).unwrap().as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0, 0,
                0,
                ptr::null_mut()
            )}?;

            // Begin connection operation on server half
            let mut overlapped: Box<OVERLAPPED> = Box::new(mem::zeroed());

            if ConnectNamedPipe(
                server_pipe.get(),
                &mut *overlapped
            ) != 0 {
                return Err(io::Error::last_os_error());
            }

            if GetLastError() != ERROR_IO_PENDING {
                return Err(io::Error::last_os_error());
            }

            Ok(NamedMessageChannel {
                name: OsString::from(pipe_name),
                server_pipe,
                overlapped,
                tokio_loop: tokio.clone(),
            })
        }
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn accept(mut self, timeout: Option<Duration>) -> io::Result<MessageChannel> {
        unsafe {
            let timeout = if let Some(duration) = timeout {
                (duration.as_secs() * 1000 + (duration.subsec_nanos() as u64 / (1000 * 1000))) as ULONG
            } else {
                INFINITE
            };
            let mut bytes: DWORD = 0;
            winapi_bool_call!(GetOverlappedResultEx(
                self.server_pipe.get(),
                &mut *self.overlapped,
                &mut bytes,
                timeout,
                TRUE
            ))?;

            let mut process_id: ULONG = 0;
            winapi_bool_call!(GetNamedPipeClientProcessId(
                self.server_pipe.get(),
                &mut process_id,
            ))?;

            let remote_process_handle = winapi_handle_call!(OpenProcess(
                PROCESS_DUP_HANDLE,
                FALSE,
                process_id,
            ))?;

            let target_state = Arc::new(Mutex::new(HandleTargetState::ClientSentTo(remote_process_handle)));

            Ok(MessageChannel {
                pipe: NamedPipe::Unregistered { pipe: self.server_pipe, tokio: self.tokio_loop },
                server: true,
                target_state,
            })
        }
    }

    pub fn connect<N>(name: N, timeout: Option<Duration>, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> where
        N: AsRef<OsStr>,
    {
        unsafe {
            let name = WString::from(name.as_ref())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid name for named pipe"))?;

            let timeout = if let Some(duration) = timeout {
                (duration.as_secs() * 1000 + (duration.subsec_nanos() as u64 / (1000 * 1000))) as DWORD
            } else {
                NMPWAIT_WAIT_FOREVER
            };

            winapi_bool_call!(WaitNamedPipeW(
                name.as_ptr(),
                timeout,
            ))?;

            let client_pipe = winapi_handle_call! { CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
                ptr::null_mut()
            )}?;

            let mut process_id: ULONG = 0;
            winapi_bool_call!(GetNamedPipeServerProcessId(
                client_pipe.get(),
                &mut process_id,
            ))?;

            let remote_process_handle = winapi_handle_call!(OpenProcess(
                PROCESS_DUP_HANDLE,
                FALSE,
                process_id,
            ))?;

            let target_state = Arc::new(Mutex::new(HandleTargetState::ServerSentTo(remote_process_handle)));

            Ok(MessageChannel {
                pipe: NamedPipe::Unregistered { pipe: client_pipe, tokio: tokio_loop.clone() },
                server: false,
                target_state,
            })
        }
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

const SECURITY_IDENTIFICATION: DWORD = 65536;
const NMPWAIT_WAIT_FOREVER: DWORD = 0xFFFFFFFF;
