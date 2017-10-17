use std::{io, fs, env};
use std::io::{Read, Write, Seek, SeekFrom};

use sandbox_ipc::{MessageChannel, SendableFile};
use sandbox_ipc::platform::{MessageChannel as OsMessageChannel};
use futures::prelude::*;

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
pub fn run_parent(channel: OsMessageChannel) -> io::Result<()> {
    let channel = MessageChannel::<Message, Message>::from_os(channel, 8192).unwrap();

    let channel = await!(channel.send(Message::Hello))?;
    println!("parent sent hello");
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::Ehlo) = message {} else {
        panic!("expected Ehlo, got {:?}", message);
    }
    println!("parent received ehlo");

    // Use child to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file.txt");
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&file_path)?;
    write!(file, "{}", STRING_WRITTEN_TO_FILE)?;
    let channel = await!(channel.send(Message::ReadAFile(file.into())))?;
    println!("parent sent file handle");
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::FileSaidThis(data)) = message {
        println!("parent received file contents");
        assert_eq!(STRING_WRITTEN_TO_FILE, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    // Read a file for child
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    let _channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        println!("parent received file handle");
        file.seek(SeekFrom::Start(0))?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        await!(channel.send(Message::FileSaidThis(data)))?
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };
    println!("parent sent file contents");

    Ok(())
}

#[async]
pub fn run_child(channel: OsMessageChannel) -> io::Result<()> {
    let channel = MessageChannel::<Message, Message>::from_os(channel, 8192).unwrap();

    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::Hello) = message { } else {
        panic!("expected Hello, got {:?}", message);
    }
    println!("child received hello");
    let channel = await!(channel.send(Message::Ehlo))?;
    println!("child sent ehlo");

    // Read a file for parent
    let (message, channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    let channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        println!("child received file handle");
        file.seek(SeekFrom::Start(0))?;
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        await!(channel.send(Message::FileSaidThis(data)))?
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };
    println!("child sent file contents");

    // Use parent to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file_two.txt");
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&file_path)?;
    write!(file, "{}", STRING_WRITTEN_TO_FILE2)?;
    let channel = await!(channel.send(Message::ReadAFile(file.into())))?;
    println!("child sent file handle");
    let (message, _channel) = await!(channel.into_future()).map_err(|(err, _)| err)?;
    if let Some(Message::FileSaidThis(data)) = message {
        println!("child received file contents");
        assert_eq!(STRING_WRITTEN_TO_FILE2, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    Ok(())
}