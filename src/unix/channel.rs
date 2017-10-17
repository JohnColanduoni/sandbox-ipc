use platform::Channel;
use super::*;

use std::{io, mem, slice, ptr, env, process};
use std::marker::PhantomData;
use std::time::Duration;
use std::sync::{Arc, Mutex};
use std::net::Shutdown;
use std::os::unix::prelude::*;
use std::os::unix::net::UnixStream;
use std::ffi::{OsString, OsStr};

use uuid::Uuid;
use serde::{Serializer, Deserializer};
use tokio::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
use futures::{Poll, Async};
use platform::tokio_uds::{UnixStream as TokioUnixStream};
use platform::libc;

macro_rules! use_seqpacket {
    () => { cfg!(not(target_os = "macos")) };
}

pub struct MessageChannel {
    socket: TokioUnixStream,
}

impl MessageChannel {
    pub fn pair(tokio_loop: &TokioHandle) -> io::Result<(Self, Self)> {
        unsafe {
            let mut sockets: [libc::c_int; 2] = [0, 0];

            let socket_type = if use_seqpacket!() {
                libc::SOCK_SEQPACKET
            } else {
                libc::SOCK_DGRAM
            };

            if libc::socketpair(libc::AF_UNIX, socket_type, 0, sockets.as_mut_ptr()) == -1 {
                return Err(io::Error::last_os_error());
            }

            for &socket in sockets.iter() {
                set_cloexec(socket)?;
            }

            Ok((
                MessageChannel { socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(sockets[0]), tokio_loop)? },
                MessageChannel { socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(sockets[1]), tokio_loop)? },
            ))
        }
    }

    pub fn send_to_child<F>(self, command: &mut process::Command, transmit_and_launch: F) -> io::Result<process::Child> where
        F: FnOnce(&mut process::Command, &ChildMessageChannel) -> io::Result<process::Child>
    {
        let fd = self.socket.as_raw_fd();

        unsafe { clear_cloexec(fd)? }

        let channel = ChildMessageChannel { fd };

        transmit_and_launch(command, &channel)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChildMessageChannel {
    fd: libc::c_int,
}

impl ChildMessageChannel {
    pub fn into_channel(self, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> {
        unsafe { Ok(
            MessageChannel { socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(self.fd), tokio_loop)? },
        )}
    }
}

impl Channel for MessageChannel {
}

impl io::Write for MessageChannel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.socket.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Datagrams don't flush
        Ok(())
    }
}

impl AsyncWrite for MessageChannel {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        AsyncWrite::shutdown(&mut self.socket)
    }
}

impl io::Read for MessageChannel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.socket.read(buffer)
    }
}

impl AsyncRead for MessageChannel {
}

pub struct NamedMessageChannel {
    socket: libc::c_int,
    tokio_loop: TokioHandle,
    name: OsString,
}

impl Drop for NamedMessageChannel {
    fn drop(&mut self) {
        unsafe { libc::close(self.socket); }
    }
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
            if use_seqpacket!() {
                let socket = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
                set_cloexec(socket)?;

                let addr = sockaddr_un(&name);
                if libc::bind(socket, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(NamedMessageChannel {
                    socket,
                    tokio_loop: tokio_loop.clone(),
                    name: OsString::from(name),
                })
            } else {
                let socket = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
                set_cloexec(socket)?;

                let addr = sockaddr_un(&name);
                if libc::bind(socket, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(NamedMessageChannel {
                    socket,
                    tokio_loop: tokio_loop.clone(),
                    name: OsString::from(name),
                })
            }
        }
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn accept(self, timeout: Option<Duration>) -> io::Result<MessageChannel> {
        // TODO: use timeout
        unsafe {
            if use_seqpacket!() {
                let socket = libc::accept(self.socket, ptr::null_mut(), ptr::null_mut());
                
                if socket < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(self.socket);

                Ok(MessageChannel {
                    socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(socket), &self.tokio_loop)?,
                })
            } else {
                let mut buffer = [0u8; 1];
                let mut cmsg = Cmsg::new_recv(1, &mut buffer[..]);
                if libc::recvmsg(self.socket, cmsg.ptr(), 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(self.socket);

                Ok(MessageChannel {
                    socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(cmsg.fds()[0]), &self.tokio_loop)?,
                })
            }
        }
    }

    pub fn connect<N>(name: N, timeout: Option<Duration>, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> where
        N: AsRef<OsStr>,
    {
        let name = name.as_ref();

        // TODO: use timeout
        unsafe {
            if use_seqpacket!() {
                let socket = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
                set_cloexec(socket)?;

                let addr = sockaddr_un(name);
                if libc::connect(socket, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(MessageChannel {
                    socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(socket), tokio_loop)?,
                })
            } else {
                let init_socket = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
                set_cloexec(init_socket)?;
                let addr = sockaddr_un(&name);
                if libc::connect(init_socket, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
                    return Err(io::Error::last_os_error());
                }

                // Create unnammed SOCK_DGRAM pair
                let mut sockets: [libc::c_int; 2] = [0, 0];
                if libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sockets.as_mut_ptr()) == -1 {
                    return Err(io::Error::last_os_error());
                }
                for &socket in sockets.iter() {
                    set_cloexec(socket)?;
                }

                let mut cmsg = Cmsg::new_send(&[sockets[1]], &[0u8]);

                if libc::sendmsg(init_socket, cmsg.ptr(), 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(sockets[1]);

                Ok(MessageChannel {
                    socket: TokioUnixStream::from_stream(UnixStream::from_raw_fd(sockets[0]), tokio_loop)?,
                })
            }
        }
    }
}

fn set_cloexec(fd: libc::c_int) -> io::Result<()> {
    unsafe {
        let old_flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if libc::fcntl(fd, libc::F_SETFD, old_flags | libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn clear_cloexec(fd: libc::c_int) -> io::Result<()> {
    unsafe {
        let old_flags = libc::fcntl(fd, libc::F_GETFD, 0);
        if libc::fcntl(fd, libc::F_SETFD, old_flags & !libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn sockaddr_un(path: &OsStr) -> libc::sockaddr_un {
    unsafe {
        let mut addr: libc::sockaddr_un = mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        assert!(path.len() < addr.sun_path.len(), "path too long for sockaddr_un");
        addr.sun_path[..path.len()].copy_from_slice(mem::transmute(path.as_bytes()));

        addr
    }
}

#[allow(non_snake_case)]
fn CMSG_LEN(length: libc::size_t) -> libc::size_t {
    CMSG_ALIGN(mem::size_of::<libc::cmsghdr>()) + length
}

#[allow(non_snake_case)]
unsafe fn CMSG_DATA(cmsg: *mut libc::cmsghdr) -> *mut libc::c_void {
    (cmsg as *mut libc::c_uchar).offset(CMSG_ALIGN(
            mem::size_of::<libc::cmsghdr>()) as isize) as *mut libc::c_void
}

#[allow(non_snake_case)]
fn CMSG_ALIGN(length: libc::size_t) -> libc::size_t {
    (length + mem::size_of::<libc::size_t>() - 1) & !(mem::size_of::<libc::size_t>() - 1)
}

#[allow(non_snake_case)]
fn CMSG_SPACE(length: libc::size_t) -> libc::size_t {
    CMSG_ALIGN(length) + CMSG_ALIGN(mem::size_of::<libc::cmsghdr>())
}

struct Cmsg<'a> {
    msg_hdr: libc::msghdr,
    buffer: Vec<u8>,
    iovec: Box<libc::iovec>,
    sockaddr: Option<Box<libc::sockaddr_un>>,
    fd_count: usize,
    _phantom: PhantomData<&'a [u8]>,
}

impl<'a> Cmsg<'a> {
    fn new_send(fds: &[libc::c_int], data: &'a [u8]) -> Self {
        unsafe {
            let fd_payload_size = mem::size_of::<libc::c_int>() * fds.len();
            let mut buffer = vec![0u8; CMSG_SPACE(fd_payload_size)];
            let mut iovec: Box<libc::iovec> = Box::new(mem::zeroed());

            iovec.iov_base = data.as_ptr() as *mut u8 as *mut libc::c_void;
            iovec.iov_len = data.len() as _;

            let cmsg_hdr = &mut *(buffer.as_mut_ptr() as *mut libc::cmsghdr);
            cmsg_hdr.cmsg_len = CMSG_LEN(fd_payload_size as _) as _;
            cmsg_hdr.cmsg_level = libc::SOL_SOCKET;
            cmsg_hdr.cmsg_type = libc::SCM_RIGHTS;

            let mut msg_hdr: libc::msghdr = mem::zeroed();
            msg_hdr.msg_name = ptr::null_mut();
            msg_hdr.msg_namelen = 0;
            msg_hdr.msg_iov = &mut *iovec;
            msg_hdr.msg_iovlen = 1;
            msg_hdr.msg_control = cmsg_hdr as *mut _ as *mut libc::c_void;
            msg_hdr.msg_controllen = cmsg_hdr.cmsg_len;
            msg_hdr.msg_flags = 0;

            let fds_target = slice::from_raw_parts_mut(CMSG_DATA(cmsg_hdr) as *mut libc::c_int, fds.len());
            fds_target.copy_from_slice(fds);

            Cmsg { msg_hdr, buffer, iovec, sockaddr: None, fd_count: fds.len(), _phantom: PhantomData }
        }
    }

    fn new_recv(fd_count: usize, data: &'a mut [u8]) -> Self {
        unsafe {
            let fd_payload_size = mem::size_of::<libc::c_int>() * fd_count;
            let mut buffer = vec![0u8; CMSG_SPACE(fd_payload_size)];
            let mut iovec: Box<libc::iovec> = Box::new(mem::zeroed());
            let mut sockaddr: Box<libc::sockaddr_un> = Box::new(mem::zeroed());

            iovec.iov_base = data.as_ptr() as *mut u8 as *mut libc::c_void;
            iovec.iov_len = data.len() as _;

            let cmsg_hdr = &mut *(buffer.as_mut_ptr() as *mut libc::cmsghdr);
            cmsg_hdr.cmsg_len = CMSG_LEN(fd_payload_size as _) as _;
            cmsg_hdr.cmsg_level = libc::SOL_SOCKET;
            cmsg_hdr.cmsg_type = libc::SCM_RIGHTS;

            let mut msg_hdr: libc::msghdr = mem::zeroed();
            msg_hdr.msg_name = &mut *sockaddr as *mut libc::sockaddr_un as _;
            msg_hdr.msg_namelen = mem::size_of::<libc::sockaddr_un>() as _;
            msg_hdr.msg_iov = &mut *iovec;
            msg_hdr.msg_iovlen = 1;
            msg_hdr.msg_control = cmsg_hdr as *mut _ as *mut libc::c_void;
            msg_hdr.msg_controllen = cmsg_hdr.cmsg_len;
            msg_hdr.msg_flags = 0;

            Cmsg { msg_hdr, buffer, iovec, sockaddr: Some(sockaddr), fd_count, _phantom: PhantomData }
        }
    }

    unsafe fn ptr(&mut self) -> *mut libc::msghdr {
        &mut self.msg_hdr
    }

    fn fds(&self) -> &[libc::c_int] {
        unsafe { slice::from_raw_parts(CMSG_DATA(self.buffer.as_ptr() as _) as *mut libc::c_int, self.fd_count) }
    }
}