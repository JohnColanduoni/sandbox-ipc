#![feature(proc_macro, conservative_impl_trait, generators)]

extern crate sandbox_ipc;
extern crate tokio_core;
extern crate futures_await as futures;

#[macro_use] extern crate serde_derive;
extern crate serde_urlencoded;

use std::{env};
use std::process::Command;

use sandbox_ipc::platform::{MessageChannel as OsMessageChannel, ChildMessageChannel as OsChildMessageChannel};
use tokio_core::reactor::{Core as TokioLoop};

const CHILD_CHANNEL_ARG: &str = "--child-channel=";

mod mp_channel_base;

fn main() {
    if let Some(arg) = env::args().find(|x| x.starts_with(CHILD_CHANNEL_ARG)) {
        let mut tokio_loop = TokioLoop::new().unwrap();
        let channel_serialized: OsChildMessageChannel = serde_urlencoded::from_str(&arg[CHILD_CHANNEL_ARG.len()..]).unwrap();
        let channel = channel_serialized.into_channel(&tokio_loop.handle()).unwrap();

        tokio_loop.run(mp_channel_base::run_child(channel)).unwrap();
    } else {
        let mut tokio_loop = TokioLoop::new().unwrap();
        let (a, b) = OsMessageChannel::pair(&tokio_loop.handle()).unwrap();

        let mut child_command = Command::new(env::current_exe().unwrap());
        let mut child = b.send_to_child(&mut child_command, |command, child_channel| {
            command.arg(format!("{}{}", CHILD_CHANNEL_ARG, serde_urlencoded::to_string(&child_channel).unwrap())).spawn()
        }).unwrap();

        tokio_loop.run(mp_channel_base::run_parent(a)).unwrap();

        if !child.wait().unwrap().success() {
            panic!("child process returned failure exit code");
        }
    }
}
