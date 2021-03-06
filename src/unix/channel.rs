use platform::Channel;
use super::*;

use std::{io, mem, ptr, env, process};
use std::time::Duration;
use std::ffi::{OsString, OsStr};

use uuid::Uuid;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
use self::mio::Ready;
use futures::{Poll, Async};
use platform::libc;

macro_rules! use_seqpacket {
    () => { cfg!(not(target_os = "macos")) };
}

pub(crate) const MAX_MSG_FDS: usize = 32;

pub struct MessageChannel {
    socket: PollEvented<ScopedFd>,
    cmsg: Cmsg,
}

// For all unixy platforms we use unix domain sockets for MessageChannel. For platforms that
// support SOCK_SEQPACKET, we use that. OS X notably doesn't, but it does preserve message
// boundaries when using sendmsg with a SOCK_STREAM socket, so we just use that.
impl MessageChannel {
    pub fn pair(tokio_loop: &TokioHandle) -> io::Result<(Self, Self)> {
        let (a, b) = raw_socketpair()?;

        Ok((
            MessageChannel { socket: PollEvented::new_with_handle(a, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
            MessageChannel { socket: PollEvented::new_with_handle(b, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
        ))
    }

    pub fn establish_with_child<F>(command: &mut process::Command, tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, process::Child)> where
        F: FnOnce(&mut process::Command, ChildMessageChannel) -> io::Result<process::Child>
    {
        let (a, b) = raw_socketpair()?;
        let _guard = CloexecGuard::new(&b.0);

        let channel = ChildMessageChannel { fd: b.0 };

        let child = transmit_and_launch(command, channel)?;

        Ok((
            MessageChannel { socket: PollEvented::new_with_handle(a, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS), },
            child,
        ))
    }

    pub fn establish_with_child_custom<F, T>(tokio_loop: &TokioHandle, transmit_and_launch: F) -> io::Result<(Self, T)> where
        F: FnOnce(ChildMessageChannel) -> io::Result<(ProcessHandle, T)>
    {
        let (a, b) = raw_socketpair()?;
        let _guard = CloexecGuard::new(&b.0);

        let channel = ChildMessageChannel { fd: b.0 };

        let (_token, t) = transmit_and_launch(channel)?;

        Ok((
            MessageChannel { socket: PollEvented::new_with_handle(a, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS), },
            t,
        ))
    }
}

impl<T, R> AsRawFd for ::MessageChannel<T, R> where
    T: Serialize,
    R: for<'a> Deserialize<'a>,
{
    fn as_raw_fd(&self) -> RawFd {
        self.inner.io.socket.get_ref().as_raw_fd()
    }
}

impl AsRawFd for ::RawMessageChannel {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket.get_ref().as_raw_fd()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChildMessageChannel {
    fd: libc::c_int,
}

impl ChildMessageChannel {
    pub fn into_channel(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        Ok(
            MessageChannel { socket: PollEvented::new_with_handle(ScopedFd(self.fd), tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
        )
    }
}

impl AsRawFd for ::ChildMessageChannel {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.fd
    }
}

impl AsRawFd for ::ChildRawMessageChannel {
    fn as_raw_fd(&self) -> RawFd {
        self.0.fd
    }
}

impl Channel for MessageChannel {
    fn cmsg(&mut self) -> &mut Cmsg {
        &mut self.cmsg
    }
}

impl io::Write for MessageChannel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Async::Ready(_) = self.socket.poll_write_ready()? {
            unsafe { 
                self.cmsg.set_data(buffer);
                let result = libc::sendmsg(self.socket.get_ref().0, self.cmsg.ptr(), libc::MSG_DONTWAIT);
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        self.socket.clear_write_ready()?;
                    }
                    Err(err)
                }
            }
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Datagrams don't flush
        Ok(())
    }
}

impl AsyncWrite for MessageChannel {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        if unsafe { libc::shutdown(self.socket.get_ref().0, libc::SHUT_RDWR) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Async::Ready(()))
    }
}

impl io::Read for MessageChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Async::Ready(_) = self.socket.poll_read_ready(Ready::readable())? {
            unsafe { 
                self.cmsg.set_data(buffer);
                self.cmsg.reset_fd_count();
                let result = libc::recvmsg(self.socket.get_ref().0, self.cmsg.ptr(), libc::MSG_DONTWAIT);
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        self.socket.clear_read_ready(Ready::readable())?;
                    }
                    Err(err)
                }
            }
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}

impl AsyncRead for MessageChannel {
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PreMessageChannel {
    fd: ScopedFd,
}

impl PreMessageChannel {
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = raw_socketpair()?;

        Ok((
            PreMessageChannel { fd: a },
            PreMessageChannel { fd: b },
        ))
    }

    pub fn into_channel(self, _remote_process: ProcessHandle, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        Ok(MessageChannel { socket: PollEvented::new_with_handle(self.fd, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) })
    }

    pub fn into_sealed_channel(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        // TODO: seal the channel
        Ok(MessageChannel { socket: PollEvented::new_with_handle(self.fd, tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) })
    }
}

impl<T, R> AsRawFd for ::PreMessageChannel<T, R> where
    T: Serialize,
    R: for<'a> Deserialize<'a>,
{
    fn as_raw_fd(&self) -> RawFd {
        self.inner.fd.as_raw_fd()
    }
}

impl AsRawFd for ::PreRawMessageChannel {
    fn as_raw_fd(&self) -> RawFd {
        self.0.fd.as_raw_fd()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessHandle {
}

impl ProcessHandle {
    pub fn current() -> io::Result<Self> {
        Ok(ProcessHandle {})
    }

    pub fn from_child(_child: &process::Child) -> io::Result<Self> {
        Ok(ProcessHandle {})
    }

    pub fn clone(&self) -> io::Result<Self> {
        Ok(ProcessHandle {})
    }
}

pub struct NamedMessageChannel {
    socket: ScopedFd,
    tokio_loop: TokioHandle,
    name: OsString,
}

// NamedMessageChannel operates in one of two ways, depending on whether SOCK_SEQPACKET is supported.
//
// If it is, it's a pretty straightforward connection to a named SOCK_SEQPACKET socket.
//
// If it isn't, we create a named SOCK_DGRAM socket for the initial connection. The connecting process creates a
// unnammed SOCK_DGRAM pair via socketpair, and then sends one end via the named SOCK_DGRAM socket.
impl NamedMessageChannel {
     pub fn new(tokio_loop: &TokioHandle) -> io::Result<Self> {
        let name = OsString::from(env::temp_dir().join(format!("{}", Uuid::new_v4())));

        unsafe {
            let typ = if use_seqpacket!() {
                libc::SOCK_SEQPACKET
            } else {
                libc::SOCK_STREAM
            };

            let socket = ScopedFd(libc::socket(libc::AF_UNIX, typ, 0));
            set_cloexec(socket.0)?;

            let addr = sockaddr_un(&name);
            if libc::bind(socket.0, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::listen(socket.0, 1) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(NamedMessageChannel {
                socket,
                tokio_loop: tokio_loop.clone(),
                name: OsString::from(name),
            })
        }
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn accept(self, _timeout: Option<Duration>) -> io::Result<MessageChannel> {
        // TODO: use timeout
        unsafe {
            let socket = libc::accept(self.socket.0, ptr::null_mut(), ptr::null_mut());
            if socket < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(MessageChannel {
                socket: PollEvented::new_with_handle(ScopedFd(socket), &self.tokio_loop)?,
                cmsg: Cmsg::new(MAX_MSG_FDS),
            })
        }
    }

    pub fn connect<N>(name: N, _timeout: Option<Duration>, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> where
        N: AsRef<OsStr>,
    {
        let name = name.as_ref();

        // TODO: use timeout
        unsafe {
            let typ = if use_seqpacket!() {
                libc::SOCK_SEQPACKET
            } else {
                libc::SOCK_STREAM
            };

            let socket = libc::socket(libc::AF_UNIX, typ, 0);
            set_cloexec(socket)?;

            let addr = sockaddr_un(name);
            if libc::connect(socket, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(MessageChannel {
                socket: PollEvented::new_with_handle(ScopedFd(socket), tokio_loop)?,
                cmsg: Cmsg::new(MAX_MSG_FDS),
            })
        }
    }
}

fn raw_socketpair() -> io::Result<(ScopedFd, ScopedFd)> {
    unsafe {
        let mut sockets: [libc::c_int; 2] = [0, 0];

        let socket_type = if use_seqpacket!() {
            libc::SOCK_SEQPACKET
        } else {
            libc::SOCK_STREAM
        };

        if libc::socketpair(libc::AF_UNIX, socket_type, 0, sockets.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error());
        }

        for &socket in sockets.iter() {
            set_cloexec(socket)?;
        }

        Ok((ScopedFd(sockets[0]), ScopedFd(sockets[1])))
    }
}

struct CloexecGuard<'a>(&'a RawFd);

impl<'a> Drop for CloexecGuard<'a> {
    fn drop(&mut self) {
        if let Err(err) = set_cloexec(*self.0) {
            error!("failed to set CLOEXEC: {}", err);
        }
    }
}

impl<'a> CloexecGuard<'a> {
    pub fn new(fd: &'a RawFd) -> io::Result<Self> {
        clear_cloexec(*fd)?;

        Ok(CloexecGuard(fd))
    }
}