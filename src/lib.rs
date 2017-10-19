#![feature(collections_range)]

#[macro_use] extern crate log;
extern crate uuid;

extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate bincode;

extern crate tokio_core;
#[macro_use] extern crate tokio_io;

mod tokio {
    pub use tokio_core::*;
    pub use tokio_io::*;
}
extern crate futures;

#[cfg(target_os = "windows")]
#[macro_use] extern crate winhandle;

mod io;
mod sync;

mod ser;

pub use io::*;
pub use sync::*;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
pub mod platform;

#[cfg(unix)]
#[path = "unix/mod.rs"]
pub mod platform;

pub use platform::{SharedMem, SharedMemMap};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SharedMemAccess {
    Read,
    ReadWrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{fs, env, thread};
    use std::io::{Write, Read, Seek, SeekFrom};

    use futures::{Sink, Stream};

    #[test]
    fn message_channel_pair() {
        let reactor = tokio::reactor::Core::new().unwrap();
        let (_a, _b) = platform::MessageChannel::pair(&reactor.handle()).unwrap();
    }

    #[test]
    fn named_message_channel_pair() {
        let reactor = tokio::reactor::Core::new().unwrap();
        let server = platform::NamedMessageChannel::new(&reactor.handle()).unwrap();

        let name = server.name().to_os_string();
        println!("named socket: {:?}", name);
        let client_thread = thread::spawn(move || {
            let reactor = tokio::reactor::Core::new().unwrap();
            let _client = platform::NamedMessageChannel::connect(&name, None, &reactor.handle()).unwrap();
        });

        let _server = server.accept(None).unwrap();
        client_thread.join().unwrap();
    }

    #[test]
    fn send_file_same_process() {
        let mut reactor = tokio::reactor::Core::new().unwrap();
        let (a, b) = MessageChannel::<SendableFile, SendableFile>::pair(&reactor.handle(), 8192).unwrap();

        let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true)
            .open(env::temp_dir().join("some_test_file.txt")).unwrap();
        write!(file, "hello").unwrap();
        file.flush().unwrap();

        let _a = reactor.run(a.send(SendableFile(file))).unwrap();
        let (message, _b) = reactor.run(b.into_future()).map_err(|(err, _)| err).unwrap();
        let SendableFile(mut file) = message.unwrap();

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut buffer = String::new();
        file.read_to_string(&mut buffer).unwrap();

        assert_eq!("hello", buffer);
    }

    #[test]
    fn send_mem_same_process() {
        let mut reactor = tokio::reactor::Core::new().unwrap();
        let (a, b) = MessageChannel::pair(&reactor.handle(), 8192).unwrap();

        let test_bytes: &[u8] = b"hello";

        let memory = platform::SharedMem::new(0x1000).unwrap();
        unsafe {
            let mapping = memory.map_ref(.., SharedMemAccess::ReadWrite).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            slice[0..test_bytes.len()].copy_from_slice(test_bytes);
        }

        let _a = reactor.run(a.send(memory)).unwrap();
        let (message, _b) = reactor.run(b.into_future()).map_err(|(err, _)| err).unwrap();
        let memory: SharedMem = message.unwrap();

        unsafe {
            let mapping = memory.map_ref(.., SharedMemAccess::Read).unwrap();
            let slice = ::std::slice::from_raw_parts_mut(mapping.pointer(), mapping.len());

            assert_eq!(&slice[0..test_bytes.len()], test_bytes);
        }
    }
}