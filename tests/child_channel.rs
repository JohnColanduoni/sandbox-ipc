extern crate sandbox_ipc;
extern crate tokio;
extern crate futures;

#[macro_use] extern crate serde_derive;
extern crate serde_json;

use std::{env};
use std::process::Command;

use sandbox_ipc::{MessageChannel, ChildMessageChannel};
use tokio::runtime::{Runtime as TokioLoop};

const CHILD_CHANNEL_ENV_VAR: &str = "CHILD_CHANNEL";

mod mp_channel_base;

fn main() {
    if let Ok(arg) = env::var(CHILD_CHANNEL_ENV_VAR) {
        let tokio_loop = TokioLoop::new().unwrap();
        let channel_serialized: ChildMessageChannel = serde_json::from_str(&arg).unwrap();
        let channel = channel_serialized.into_channel(tokio_loop.reactor()).unwrap();

        mp_channel_base::run_child(tokio_loop, channel);
    } else {
        let tokio_loop = TokioLoop::new().unwrap();

        let mut child_command = Command::new(env::current_exe().unwrap());
        let (a, mut child) = MessageChannel::establish_with_child(&mut child_command, 8192, tokio_loop.reactor(), |command, child_channel| {
            command.env(CHILD_CHANNEL_ENV_VAR, serde_json::to_string(child_channel).unwrap()).spawn()
        }).unwrap();

        mp_channel_base::run_parent(tokio_loop, a);

        if !child.wait().unwrap().success() {
            panic!("child process returned failure exit code");
        }
    }
}
