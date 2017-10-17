#![feature(proc_macro, conservative_impl_trait, generators)]

extern crate sandbox_ipc;
extern crate tokio_core;
extern crate futures_await as futures;

#[macro_use] extern crate serde_derive;
extern crate serde_urlencoded;

use std::{io, fs, env};
use std::process::Command;
use std::io::{Read, Write};

use sandbox_ipc::{MessageChannel, SendableFile};
use sandbox_ipc::platform::{MessageChannel as OsMessageChannel, ChildMessageChannel as OsChildMessageChannel};
use tokio_core::reactor::{Core as TokioLoop};
use futures::prelude::*;

const CHILD_CHANNEL_ARG: &str = "--child-channel=";

fn main() {
    if let Some(arg) = env::args().find(|x| x.starts_with(CHILD_CHANNEL_ARG)) {
        let mut tokio_loop = TokioLoop::new().unwrap();
        let channel_serialized: OsChildMessageChannel = serde_urlencoded::from_str(&arg[CHILD_CHANNEL_ARG.len()..]).unwrap();
        let channel = channel_serialized.into_channel(&tokio_loop.handle()).unwrap();

        tokio_loop.run(run_child(channel)).unwrap();
    } else {
        let mut tokio_loop = TokioLoop::new().unwrap();
        let (a, b) = OsMessageChannel::pair(&tokio_loop.handle()).unwrap();

        let mut child_command = Command::new(env::current_exe().unwrap());
        let mut child = b.send_to_child(&mut child_command, |command, child_channel| {
            command.arg(format!("{}{}", CHILD_CHANNEL_ARG, serde_urlencoded::to_string(&child_channel).unwrap())).spawn()
        }).unwrap();

        tokio_loop.run(run_parent(a)).unwrap();

        if !child.wait().unwrap().success() {
            panic!("child process returned failure exit code");
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Hello,
    Ehlo,

    ReadAFile(SendableFile),
    FileSaidThis(String),
}

const STRING_WRITTEN_TO_FILE: &str = "this is in a file";
const STRING_WRITTEN_TO_FILE2: &str = "this is in another file";

#[async]
fn run_parent(channel: OsMessageChannel) -> io::Result<()> {
    let channel = MessageChannel::<Message, Message>::from_os(channel, 8192).unwrap();

    let channel = await!(channel.send(Message::Hello))?;
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::Ehlo) = message {} else {
        panic!("expected Ehlo, got {:?}", message);
    }

    // Use child to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file.txt");
    {
        let mut file = fs::File::create(&file_path)?;
        write!(file, "{}", STRING_WRITTEN_TO_FILE)?;
    }
    let channel = await!(channel.send(Message::ReadAFile(fs::File::open(&file_path)?.into())))?;
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::FileSaidThis(data)) = message {
        assert_eq!(STRING_WRITTEN_TO_FILE, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    // Read a file for child
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    let _channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        await!(channel.send(Message::FileSaidThis(data)))?
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };

    Ok(())
}

#[async]
fn run_child(channel: OsMessageChannel) -> io::Result<()> {
    let channel = MessageChannel::<Message, Message>::from_os(channel, 8192).unwrap();

    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::Hello) = message { } else {
        panic!("expected Hello, got {:?}", message);
    }
    let channel = await!(channel.send(Message::Ehlo))?;

    // Read a file for parent
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    let channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        await!(channel.send(Message::FileSaidThis(data)))?
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };

    // Use parent to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file_two.txt");
    {
        let mut file = fs::File::create(&file_path)?;
        write!(file, "{}", STRING_WRITTEN_TO_FILE2)?;
    }
    let channel = await!(channel.send(Message::ReadAFile(fs::File::open(&file_path)?.into())))?;
    let (message, _channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::FileSaidThis(data)) = message {
        assert_eq!(STRING_WRITTEN_TO_FILE2, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    Ok(())
}