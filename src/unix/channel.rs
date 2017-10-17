use platform::Channel;
use super::*;

use std::{io, mem, ptr, env, process};
use std::time::Duration;
use std::ffi::{OsString, OsStr};

use uuid::Uuid;
use tokio::{AsyncRead, AsyncWrite};
use tokio::reactor::{PollEvented, Handle as TokioHandle};
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
                MessageChannel { socket: PollEvented::new(ScopedFd(sockets[0]), tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
                MessageChannel { socket: PollEvented::new(ScopedFd(sockets[1]), tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
            ))
        }
    }

    pub fn send_to_child<F>(self, command: &mut process::Command, transmit_and_launch: F) -> io::Result<process::Child> where
        F: FnOnce(&mut process::Command, &ChildMessageChannel) -> io::Result<process::Child>
    {
        let fd = self.socket.get_ref().0;
        clear_cloexec(fd)?;

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
        Ok(
            MessageChannel { socket: PollEvented::new(ScopedFd(self.fd), tokio_loop)?, cmsg: Cmsg::new(MAX_MSG_FDS) },
        )
    }
}

impl Channel for MessageChannel {
    fn cmsg(&mut self) -> &mut Cmsg {
        &mut self.cmsg
    }
}

impl io::Write for MessageChannel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Async::Ready(()) = self.socket.poll_write() {
            unsafe { 
                self.cmsg.set_data(buffer);
                let result = libc::sendmsg(self.socket.get_ref().0, self.cmsg.ptr(), 0);
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        self.socket.need_write();
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
        if let Async::Ready(()) = self.socket.poll_read() {
            unsafe { 
                self.cmsg.set_data(buffer);
                self.cmsg.reset_fd_count();
                let result = libc::recvmsg(self.socket.get_ref().0, self.cmsg.ptr(), 0);
                if result >= 0 {
                    Ok(result as usize)
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        self.socket.need_read();
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
            if use_seqpacket!() {
                let socket = ScopedFd(libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0));
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
            } else {
                let socket = ScopedFd(libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0));
                set_cloexec(socket.0)?;

                let addr = sockaddr_un(&name);
                if libc::bind(socket.0, &addr as *const _ as *const libc::sockaddr, mem::size_of_val(&addr) as _) < 0 {
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

    pub fn accept(self, _timeout: Option<Duration>) -> io::Result<MessageChannel> {
        // TODO: use timeout
        unsafe {
            if use_seqpacket!() {
                let socket = libc::accept(self.socket.0, ptr::null_mut(), ptr::null_mut());
                if socket < 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(MessageChannel {
                    socket: PollEvented::new(ScopedFd(socket), &self.tokio_loop)?,
                    cmsg: Cmsg::new(MAX_MSG_FDS),
                })
            } else {
                let mut buffer = [0u8; 1];
                let mut cmsg = Cmsg::new(1);
                cmsg.set_data(&mut buffer);
                if libc::recvmsg(self.socket.0, cmsg.ptr(), 0) < 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(MessageChannel {
                    socket: PollEvented::new(ScopedFd(cmsg.fds()[0]), &self.tokio_loop)?,
                    cmsg: Cmsg::new(MAX_MSG_FDS),
                })
            }
        }
    }

    pub fn connect<N>(name: N, _timeout: Option<Duration>, tokio_loop: &TokioHandle) -> io::Result<MessageChannel> where
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
                    socket: PollEvented::new(ScopedFd(socket), tokio_loop)?,
                    cmsg: Cmsg::new(MAX_MSG_FDS),
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

                let mut cmsg = Cmsg::new(1);
                cmsg.set_fds(&[sockets[1]]);
                cmsg.set_data(&[0u8]);
                if libc::sendmsg(init_socket, cmsg.ptr(), 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(sockets[1]);

                Ok(MessageChannel {
                    socket: PollEvented::new(ScopedFd(sockets[0]), tokio_loop)?,
                    cmsg: Cmsg::new(MAX_MSG_FDS),
                })
            }
        }
    }
}
