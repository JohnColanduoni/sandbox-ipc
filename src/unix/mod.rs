use ::SendableFile;
use ser::{SerializeWrapper, SerializeWrapperGuard};

extern crate libc;
extern crate mio;

mod channel;
mod sharedmem;
mod sync;
mod fd;

pub use self::channel::*;
pub(crate) use self::sharedmem::*;
pub(crate) use self::sync::*;
use self::fd::*;

use std::{io, fs, mem, ptr, slice};
use std::cell::RefCell;
use std::borrow::Borrow;
use std::ffi::OsStr;
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
            let index = sender.send(self.0)
                .map_err(|err| S::Error::custom(err))?;
            usize::serialize(&index, serializer)
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
            let receiver = receiver_guard.as_mut()
                .ok_or_else(|| D::Error::custom("attempted to deserialize file descriptor outside of IPC channel"))?;
            let index = usize::deserialize(deserializer)?;
            let fd = receiver.recv(index)
                .map_err(|err| D::Error::custom(err))?;
            Ok(SendableFd(fd))
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

pub(crate) struct ChannelSerializeWrapper;

impl<'a, C> SerializeWrapper<'a, C> for ChannelSerializeWrapper where
    C: Channel,
{
    type SerializeGuard = ChannelSerializeGuard<'a>;
    type DeserializeGuard = ChannelDeserializeGuard;

    fn before_serialize(channel: &'a mut C) -> ChannelSerializeGuard {
        let sender = HandleSender {
            fds: [0; MAX_MSG_FDS],
            index: 0,
        };
        let mut sender = Some(sender);

        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x| 
            mem::swap(
                &mut *x.borrow_mut(),
                &mut sender,
            )
        );
        ChannelSerializeGuard(sender, channel.cmsg())
    }

    fn before_deserialize(channel: &'a mut C) -> ChannelDeserializeGuard {
        let mut receiver = HandleReceiver {
            fds: [0; MAX_MSG_FDS],
            fds_count: channel.cmsg().current_fd_count(),
        };
        receiver.fds[..receiver.fds_count].copy_from_slice(channel.cmsg().fds());
        let mut receiver = Some(receiver);

        CURRENT_DESERIALIZE_CHANNEL_REMOTE_PROCESS.with(|x|
            mem::swap(
                &mut *x.borrow_mut(),
                &mut receiver,
            )
        );
        ChannelDeserializeGuard(receiver)
    }
}

pub(crate) struct ChannelSerializeGuard<'a>(Option<HandleSender>, &'a mut Cmsg);

impl<'a> Drop for ChannelSerializeGuard<'a> {
    fn drop(&mut self) {
        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            *sender_guard = self.0.take();
        });
    }
}

impl<'a> SerializeWrapperGuard<'a> for ChannelSerializeGuard<'a> {
    fn commit(self) {
        CURRENT_SERIALIZE_CHANNEL_REMOTE_PROCESS.with(|sender_guard| {
            let mut sender_guard = sender_guard.borrow_mut();
            let sender = sender_guard.as_mut().unwrap();
            self.1.set_fds(&sender.fds[..sender.index]);
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
    fn cmsg(&mut self) -> &mut Cmsg;
}

struct HandleSender {
    fds: [libc::c_int; MAX_MSG_FDS],
    index: usize,
}

struct HandleReceiver {
    fds: [libc::c_int; MAX_MSG_FDS],
    fds_count: usize,
}

impl HandleSender {
    fn send(&mut self, fd: libc::c_int) -> io::Result<usize> {
        let index = self.index;

        if index >= self.fds.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many file descriptors in message destined for channel"));
        }

        // We need to clone fds in case they are dropped before the message is sent
        let new_fd = unsafe { libc::dup(fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        set_cloexec(new_fd)?;
        // FIXME: close dup fds

        self.fds[index] = new_fd;
        self.index += 1;
        Ok(index)
    }
}

impl HandleReceiver {
    fn recv(&self, index: usize) -> io::Result<libc::c_int> {
        if index >= self.fds_count {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "out of range file descriptor index"));
        }
        let fd = self.fds[index];

        if unsafe { libc::fcntl(fd, libc::F_GETFD, 0) } < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid file descriptor received"));
        }

        set_cloexec(fd)?;

        Ok(fd)
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

pub(crate) struct Cmsg {
    msg_hdr: libc::msghdr,
    buffer: Vec<u8>,
    iovec: Box<libc::iovec>,
    fd_count: usize,
}

impl Cmsg {
    fn new(fd_count: usize) -> Self {
        unsafe {
            let fd_payload_size = mem::size_of::<libc::c_int>() * fd_count;
            let mut buffer = vec![0u8; CMSG_SPACE(fd_payload_size)];
            let mut iovec: Box<libc::iovec> = Box::new(mem::zeroed());

            iovec.iov_base = ptr::null_mut();
            iovec.iov_len = 0;

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

            Cmsg { msg_hdr, buffer, iovec, fd_count }
        }
    }

    unsafe fn set_data(&mut self, bytes: &[u8]) {
        self.iovec.iov_base = bytes.as_ptr() as *mut _;
        self.iovec.iov_len = bytes.len() as _;
    }

    fn set_fds(&mut self, fds: &[libc::c_int]) {
        assert!(fds.len() <= self.fd_count, "insufficient space for file descriptors in cmsg");

        let cmsg_hdr = unsafe { &mut *(self.buffer.as_mut_ptr() as *mut libc::cmsghdr) };
        let fd_payload_size = mem::size_of::<libc::c_int>() * fds.len();
        cmsg_hdr.cmsg_len = CMSG_LEN(fd_payload_size as _) as _;
        self.msg_hdr.msg_controllen = cmsg_hdr.cmsg_len;

        let fds_dest = unsafe { slice::from_raw_parts_mut(CMSG_DATA(self.buffer.as_ptr() as _) as *mut libc::c_int, fds.len()) };
        fds_dest.copy_from_slice(fds);
    }

    fn reset_fd_count(&mut self) {
        let cmsg_hdr = unsafe { &mut *(self.buffer.as_mut_ptr() as *mut libc::cmsghdr) };
        let fd_payload_size = mem::size_of::<libc::c_int>() * self.fd_count;
        cmsg_hdr.cmsg_len = CMSG_LEN(fd_payload_size as _) as _;
        self.msg_hdr.msg_controllen = cmsg_hdr.cmsg_len;
    }

    fn current_fd_count(&self) -> usize {
        let cmsg_hdr = unsafe { &*(self.buffer.as_ptr() as *const libc::cmsghdr) };
        (cmsg_hdr.cmsg_len as usize - CMSG_ALIGN(mem::size_of::<libc::cmsghdr>())) / mem::size_of::<libc::c_int>()
    }

    unsafe fn ptr(&mut self) -> *mut libc::msghdr {
        &mut self.msg_hdr
    }

    fn fds(&self) -> &[libc::c_int] {
        unsafe { slice::from_raw_parts(CMSG_DATA(self.buffer.as_ptr() as _) as *mut libc::c_int, self.current_fd_count()) }
    }
}
