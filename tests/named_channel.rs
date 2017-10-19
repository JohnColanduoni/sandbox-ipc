extern crate sandbox_ipc;
extern crate tokio_core;
extern crate futures;

#[macro_use] extern crate serde_derive;

use std::{env};
use std::process::Command;
use std::ffi::OsStr;

use sandbox_ipc::{NamedMessageChannel};
use tokio_core::reactor::{Core as TokioLoop};

const CHILD_CHANNEL_ARG: &str = "--child-channel=";

mod mp_channel_base;

fn main() {
    if let Some(arg) = env::args().find(|x| x.starts_with(CHILD_CHANNEL_ARG)) {
        let tokio_loop = TokioLoop::new().unwrap();
        let channel_name = OsStr::new(&arg[CHILD_CHANNEL_ARG.len()..]);
        let channel = NamedMessageChannel::connect(&channel_name, None, &tokio_loop.handle(), 8192).unwrap();

        mp_channel_base::run_child(tokio_loop, channel);
    } else {
        let tokio_loop = TokioLoop::new().unwrap();
        let channel_server = NamedMessageChannel::new(&tokio_loop.handle(), 8192).unwrap();

        let mut child = Command::new(env::current_exe().unwrap())
            .arg(format!("{}{}", CHILD_CHANNEL_ARG, channel_server.name().to_str().unwrap()))
            .spawn().unwrap();

        let channel = channel_server.accept(None).unwrap();

        mp_channel_base::run_parent(tokio_loop, channel);

        if !child.wait().unwrap().success() {
            panic!("child process returned failure exit code");
        }
    }
}
