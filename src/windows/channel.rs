use super::{Channel, SendableWinHandle};

use std::{io, mem, ptr, process};
use std::time::Duration;
use std::os::windows::prelude::*;
use std::ffi::{OsString, OsStr};

use uuid::Uuid;
use serde::{Serializer, Deserializer};
use tokio::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
use futures::{Poll};
use platform::mio_named_pipes::NamedPipe as MioNamedPipe;
use winapi::shared::minwindef::{DWORD, ULONG, BOOL, TRUE, FALSE};
use winapi::shared::ntdef::{HANDLE};
use winapi::shared::winerror::{ERROR_IO_PENDING};
use winapi::um::minwinbase::{OVERLAPPED, SECURITY_ATTRIBUTES};
use winapi::um::winbase::{PIPE_TYPE_MESSAGE, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_ACCESS_DUPLEX, FILE_FLAG_OVERLAPPED, INFINITE};
use winapi::um::winbase::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use winapi::um::winnt::{PROCESS_DUP_HANDLE, GENERIC_READ, GENERIC_WRITE, PSECURITY_DESCRIPTOR, PACL};
use winapi::um::errhandlingapi::{GetLastError};
use winapi::um::processthreadsapi::{GetCurrentProcess, GetCurrentProcessId, GetProcessId, OpenProcess};
use winapi::um::namedpipeapi::{ConnectNamedPipe, WaitNamedPipeW, CreateNamedPipeW};
use winapi::um::fileapi::{OPEN_EXISTING, CreateFileW};
use winapi::um::ioapiset::{GetOverlappedResultEx};
use winhandle::*;
use widestring::WideCString;

#[derive(Debug)]
pub(crate) struct MessageChannel {
    pipe: PollEvented<MioNamedPipe>,
    server: bool,
    target: HandleTarget,
}

#[derive(Debug)]
pub(crate) enum HandleTarget<H = ProcessHandle> {
    #[allow(dead_code)]
    None,
    CurrentProcess,
    RemoteProcess(H),
}

impl MessageChannel {
    pub fn pair(tokio_loop: &TokioHandle) -> io::Result<(MessageChannel, MessageChannel)> {
        let (server_pipe, client_pipe) = raw_pipe_pair(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS)?;

        Ok((
            MessageChannel {
                pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(server_pipe.into_raw()) }, tokio_loop)?,
                server: true,
                target: HandleTarget::CurrentProcess,
            },
            MessageChannel {
                pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(client_pipe.into_raw()) }, tokio_loop)?,
                server: false,
                target: HandleTarget::CurrentProcess,
            },
        ))
    }

    pub fn establish_with_child<F>(command: &mut process::Command, tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, process::Child)> where
        F: FnOnce(&mut process::Command, ChildMessageChannel) -> io::Result<process::Child>
    {
        Self::establish_with_child_custom(tokio_loop, |to_be_sent| {
            let child = transmit_and_launch(command, to_be_sent)?;
            Ok((::ProcessHandle::from_windows_handle(&child)?.0, child))
        })
    }

    pub fn establish_with_child_custom<F, T>(tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, T)> where
        F: FnOnce(ChildMessageChannel) -> io::Result<(ProcessHandle, T)>,
    {
        let (server_pipe, client_pipe) = raw_pipe_pair(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS)?;
        let inheritable_process_handle = WinHandle::from_raw(unsafe { GetCurrentProcess() }).unwrap().clone_ex(true, ClonedHandleAccess::Explicit(PROCESS_DUP_HANDLE))?;
        let to_be_sent = ChildMessageChannel {
            channel_handle: client_pipe.modify(true, ClonedHandleAccess::Same)?,
            remote_process_handle: Some(inheritable_process_handle),
            remote_process_id: unsafe { GetCurrentProcessId() },
        };

        let (child_handle, t) = transmit_and_launch(to_be_sent)?;
        let channel = MessageChannel {
            pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(server_pipe.into_raw()) }, tokio_loop)?,
            server: true,
            target: HandleTarget::RemoteProcess(child_handle),
        };

        Ok((channel, t))
    }
}

impl Channel for MessageChannel {
    fn handle_target(&self) -> HandleTarget<&ProcessHandle> {
        match self.target {
            HandleTarget::None => HandleTarget::None,
            HandleTarget::CurrentProcess => HandleTarget::CurrentProcess,
            HandleTarget::RemoteProcess(ref p) => HandleTarget::RemoteProcess(p),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChildMessageChannel {
    #[serde(with = "inheritable_channel_serialize")]
    channel_handle: WinHandle,
    #[serde(with = "inheritable_channel_serialize_opt")]
    remote_process_handle: Option<WinHandle>,
    remote_process_id: DWORD,
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

mod inheritable_channel_serialize_opt {
    use super::*;

    use serde::{Serialize, Deserialize};

    pub fn serialize<S>(handle: &Option<WinHandle>, serializer: S) -> Result<S::Ok, S::Error> where
        S: Serializer,
    {
        Option::<usize>::serialize(&handle.as_ref().map(|handle|(handle.get() as usize)), serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<WinHandle>, D::Error> where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        if let Some(handle) = Option::<usize>::deserialize(deserializer)? {
            Ok(Some(WinHandle::from_raw(handle as HANDLE)
                .ok_or_else(|| D::Error::custom(io::Error::new(io::ErrorKind::InvalidData, "received invalid inherited handle")))?
            ))
        } else {
            Ok(None)
        }
    }
}

impl ChildMessageChannel {
    pub fn into_channel(self, tokio: &TokioHandle) -> io::Result<MessageChannel> {
        Ok(
            MessageChannel {
                pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(self.channel_handle.into_raw()) }, tokio)?,
                server: false,
                target: if let Some(handle) = self.remote_process_handle {
                    HandleTarget::RemoteProcess(ProcessHandle {
                        handle: SendableWinHandle(handle),
                        id: self.remote_process_id,
                    })
                } else {
                    HandleTarget::None
                },
            }
        )
    }
}

pub trait ChildRawMessageChannelExt {
    fn handles(&self) -> ChildMessageChannelHandles;
}

impl ChildRawMessageChannelExt for ::ChildRawMessageChannel {
    fn handles(&self) -> ChildMessageChannelHandles {
        ChildMessageChannelHandles { channel_handle: &self.0, index: 0 }
    }
}

pub trait ChildMessageChannelExt {
    fn handles(&self) -> ChildMessageChannelHandles;
}

impl ChildMessageChannelExt for ::ChildMessageChannel {
    fn handles(&self) -> ChildMessageChannelHandles {
        ChildMessageChannelHandles { channel_handle: &self.inner, index: 0 }
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
            0 => Some(self.channel_handle.channel_handle.get()),
            1 => self.channel_handle.remote_process_handle.as_ref().map(|x| x.get()),
            _ => return None,
        };
        self.index += 1;
        handle
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct PreMessageChannel {
    pipe: SendableWinHandle,
    server: bool,
}

impl PreMessageChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (server_pipe, client_pipe) = raw_pipe_pair(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS)?;

        Ok((
            PreMessageChannel { pipe: SendableWinHandle(server_pipe), server: true },
            PreMessageChannel { pipe: SendableWinHandle(client_pipe), server: false },
        ))
    }

    pub fn into_channel(self, process_handle: ProcessHandle, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        Ok(MessageChannel {
            pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(self.pipe.0.into_raw()) }, tokio_loop)?,
            server: self.server,
            target: HandleTarget::RemoteProcess(process_handle),
        })
    }

    pub fn into_sealed_channel(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        Ok(MessageChannel {
            pipe: PollEvented::new(unsafe { MioNamedPipe::from_raw_handle(self.pipe.0.into_raw()) }, tokio_loop)?,
            server: self.server,
            target: HandleTarget::None,
        })
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct ProcessHandle {
    pub(crate) handle: SendableWinHandle,
    pub(crate) id: DWORD,
}

impl ProcessHandle {
    pub fn current() -> io::Result<Self> {
        let handle = WinHandle::from_raw(unsafe { GetCurrentProcess() }).unwrap()
            .clone_ex(false, ClonedHandleAccess::Explicit(PROCESS_DUP_HANDLE))?;
        let id = unsafe { GetCurrentProcessId() };
        Ok(ProcessHandle {
            handle: SendableWinHandle(handle),
            id,
        })
    }

    pub fn clone(&self) -> io::Result<Self> {
        Ok(ProcessHandle {
            handle: SendableWinHandle(self.handle.0.clone()?),
            id: self.id,
        })
    }

    pub fn from_child(child: &process::Child) -> io::Result<Self> {
        Ok(ProcessHandle {
            handle: SendableWinHandle(WinHandle::clone_from(child)?),
            id: child.id(),
        })
    }
}

pub trait ProcessHandleExt: Sized {
    fn from_windows_handle<H>(handle: &H) -> io::Result<Self> where H: AsRawHandle;
    unsafe fn from_windows_handle_raw(handle: HANDLE) -> io::Result<Self>;
}

impl ProcessHandleExt for ::ProcessHandle {
    /// Creates a `ProcessHandle` from a raw Windows process handle.
    /// 
    /// The handle must have the PROCESS_DUP_HANDLE and PROCESS_QUERY_INFORMATION access rights. The handle
    /// need not stay open.
    fn from_windows_handle<H>(handle: &H) -> io::Result<Self> where H: AsRawHandle {
        unsafe { Self::from_windows_handle_raw(handle.as_raw_handle()) }
    }

    /// Creates a `ProcessHandle` from a raw Windows process handle.
    /// 
    /// The handle must have the PROCESS_DUP_HANDLE and PROCESS_QUERY_INFORMATION access rights. The handle
    /// need not stay open.
    unsafe fn from_windows_handle_raw(handle: HANDLE) -> io::Result<Self> {
        let id = GetProcessId(handle);
        if id == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(::ProcessHandle(ProcessHandle {
            handle: SendableWinHandle(WinHandle::clone_from_raw_ex(handle, false, ClonedHandleAccess::Explicit(PROCESS_DUP_HANDLE))?),
            id,
        }))
    }
}

pub(crate) struct NamedMessageChannel {
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
                WideCString::from_str(&pipe_name).unwrap().as_ptr(),
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

            let remote_process_handle = ProcessHandle {
                handle: SendableWinHandle(remote_process_handle),
                id: process_id as DWORD, 
            };

            Ok(MessageChannel {
                pipe: PollEvented::new(MioNamedPipe::from_raw_handle(self.server_pipe.into_raw()), &self.tokio_loop)?,
                server: true,
                target: HandleTarget::RemoteProcess(remote_process_handle),
            })
        }
    }

    pub fn connect<N>(name: N, timeout: Option<Duration>, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> where
        N: AsRef<OsStr>,
    {
        unsafe {
            let name = WideCString::from_str(name.as_ref())
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

            let remote_process_handle = ProcessHandle {
                handle: SendableWinHandle(remote_process_handle),
                id: process_id,
            };

            Ok(MessageChannel {
                pipe: PollEvented::new(MioNamedPipe::from_raw_handle(client_pipe.into_raw()), tokio_loop)?,
                server: false,
                target: HandleTarget::RemoteProcess(remote_process_handle),
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

fn raw_pipe_pair(pipe_type: DWORD) -> io::Result<(WinHandle, WinHandle)> {
    unsafe {
        let pipe_name = OsString::from(format!(r#"\\.\pipe\{}"#, Uuid::new_v4()));

        // Give pipe a null security descriptor
        let mut security_descriptor: [u8; 256] = mem::zeroed(); // TODO: don't fudge this structure
        winapi_bool_call!(log: InitializeSecurityDescriptor(security_descriptor.as_mut_ptr() as _, 1))?;
        winapi_bool_call!(log: SetSecurityDescriptorDacl(security_descriptor.as_mut_ptr() as _, TRUE, ptr::null_mut(), FALSE))?;

        let mut security_attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD,
            lpSecurityDescriptor: security_descriptor.as_mut_ptr() as _,
            bInheritHandle: FALSE,
        };

        debug!("creating named pipe pair {:?}", pipe_name);
        let server_pipe = winapi_handle_call! { CreateNamedPipeW(
            WideCString::from_str(&pipe_name).unwrap().as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            pipe_type,
            1,
            0, 0,
            0,
            &mut security_attributes,
        )}?;

        // Begin connection operation on server half
        let mut overlapped: OVERLAPPED = mem::zeroed();

        // This should not succeed (we are doing overlapped IO)
        if ConnectNamedPipe(
            server_pipe.get(),
            &mut overlapped
        ) != 0 {
            return Err(io::Error::last_os_error());
        }

        if GetLastError() != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }


        let client_pipe = winapi_handle_call!(log: CreateFileW(
            WideCString::from_str(&pipe_name).unwrap().as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &mut security_attributes,
            OPEN_EXISTING,
            SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
            ptr::null_mut()
        ))?;

        let mut bytes: DWORD = 0;
        winapi_bool_call!(log: GetOverlappedResultEx(
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

extern "system" {
    fn InitializeSecurityDescriptor(desc: PSECURITY_DESCRIPTOR, revision: DWORD) -> BOOL;
    fn SetSecurityDescriptorDacl(desc: PSECURITY_DESCRIPTOR, bDaclPresent: BOOL, pDacl: PACL, bDaclDefaulted: BOOL) -> BOOL;
}
