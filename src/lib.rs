//! Convenient and powerful cross-platform IPC primitives, designed with privilege separation in mind.
//!
//! The core of this crate is the `MessageChannel`. It not only automatically serializes and deserializes messages via
//! `serde`, but can even send OS resources like files to other processes. To get started, spawn a child process with
//! `MessageChannel::establish_with_child` and transmit the initial channel with an environment variable:
//!  
//! ```rust,no_run
//! extern crate futures;
//! extern crate tokio;
//! extern crate sandbox_ipc as ipc;
//! extern crate serde_json as json;
//! 
//! use std::{fs, env};
//! use std::process::Command;
//! use std::io::Write;
//!
//! use ipc::io::SendableFile;
//! use futures::prelude::*; 
//! use tokio::runtime::Runtime;
//! 
//! const CHANNEL_ENV_VAR: &str = "ENV_IPC_CHANNEL";
//! 
//! fn main() {
//!     // IO operations are done within a Tokio event loop
//!     let mut core = Runtime::new().unwrap();
//!     
//!     let mut child_command = Command::new("some_child_executable");
//!     let (channel, child) = ipc::MessageChannel::<SendableFile, i32>::establish_with_child(
//!         &mut child_command, 8192, core.reactor(), |command, child_channel| {
//!             command
//!                 .env(CHANNEL_ENV_VAR, json::to_string(child_channel).unwrap())
//!                 .spawn()
//!         }
//!     ).unwrap();
//! 
//!     let secret_file = fs::File::create("secret_file.txt").unwrap();
//!     let channel = core.block_on(channel.send(SendableFile(secret_file))).unwrap();
//! 
//!     let (reason, _channel) = core.block_on(channel.into_future()).map_err(|(err, _)| err).unwrap();
//!     let reason = reason.unwrap();
//!     assert_eq!(42i32, reason);
//! }
//! 
//! fn child_main() {
//!     let mut core = Runtime::new().unwrap();
//! 
//!     let channel: ipc::ChildMessageChannel = 
//!         json::from_str(&env::var(CHANNEL_ENV_VAR).unwrap()).unwrap();
//!     let channel = channel.into_channel::<i32, SendableFile>(core.reactor()).unwrap();
//! 
//!     let (secret_file, channel) = core.block_on(channel.into_future())
//!         .map_err(|(err, _)| err).unwrap();
//!     let SendableFile(mut secret_file) = secret_file.unwrap();
//! 
//!     write!(&mut secret_file, "psst").unwrap();
//! 
//!     let _channel = core.block_on(channel.send(42i32)).unwrap();
//! }
//! ```
//! 

#[macro_use] extern crate log;
extern crate uuid;
extern crate rand;
#[macro_use] extern crate lazy_static;

extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate bincode;

extern crate tokio_reactor;
#[macro_use] extern crate tokio_io;

mod tokio {
    pub(crate) use ::tokio_reactor as reactor;
    pub(crate) use ::tokio_io as io;
    #[cfg(test)]
    pub(crate) use ::tokio_all::runtime;
}
extern crate futures;

#[cfg(target_os = "windows")]
extern crate winapi;
#[cfg(target_os = "windows")]
#[macro_use] extern crate winhandle;
#[cfg(target_os = "windows")]
extern crate widestring;

#[cfg(test)]
extern crate tokio as tokio_all;

mod channel;
pub mod io;
pub mod sync;
pub mod shm;

mod ser;

pub use channel::*;

pub mod os {
    #[cfg(target_os = "windows")]
    pub mod windows {
        pub use platform::*;
    }
}

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(unix)]
#[path = "unix/mod.rs"]
mod platform;

#[inline]
#[cfg(test)] // Temporary, while Queue isn't well tested
fn align(x: usize, y: usize) -> usize {
    if x > 0 && y > 0 {
        (x + (y - 1)) & !(y - 1)
    } else {
        0
    }
}

#[cfg(test)]
fn check_send(_t: &Send) {
}

// TODO: specialize for platform
#[cfg(any(test, target_os = "windows"))] // Temporary, while Queue isn't well tested
const CACHE_LINE: usize = 64;

#[cfg(target_pointer_width = "32")]
const USIZE_SIZE: usize = 4;
#[cfg(target_pointer_width = "64")]
const USIZE_SIZE: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    use std::{thread};

    use tokio::reactor::Reactor;

    #[test]
    fn raw_message_channel_pair() {
        let reactor = Reactor::new().unwrap();
        let (_a, _b) = RawMessageChannel::pair(&reactor.handle()).unwrap();
    }

    #[test]
    fn named_message_channel_pair() {
        let reactor = Reactor::new().unwrap();
        let server = platform::NamedMessageChannel::new(&reactor.handle()).unwrap();

        let name = server.name().to_os_string();
        println!("named socket: {:?}", name);
        let client_thread = thread::spawn(move || {
            let reactor = Reactor::new().unwrap();
            let _client = platform::NamedMessageChannel::connect(&name, None, &reactor.handle()).unwrap();
        });

        let _server = server.accept(None).unwrap();
        client_thread.join().unwrap();
    }
}
