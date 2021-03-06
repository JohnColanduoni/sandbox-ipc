use std::{mem, fs, env, thread};
use std::io::{Read, Write, Seek, SeekFrom};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{Ordering, AtomicBool};
use std::time::Duration;

use sandbox_ipc::{MessageChannel};
use sandbox_ipc::io::{SendableFile, SendableSocket};
use sandbox_ipc::sync::{Mutex as IpcMutex, MutexHandle as IpcMutexHandle, MUTEX_SHM_SIZE};
use sandbox_ipc::shm::{SharedMem, Access as SharedMemAccess};
use futures::prelude::*;
use tokio::runtime::{Runtime as TokioLoop};

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Hello,
    Ehlo,

    ReadAFile(SendableFile),
    FileSaidThis(String),

    HaveASocket(SendableSocket<TcpStream>),

    HaveAMutex(SharedMem, IpcMutexHandle),
    WroteToSharedMem,
}

const STRING_WRITTEN_TO_FILE: &str = "this is in a file";
const STRING_WRITTEN_TO_FILE2: &str = "this is in another file";

macro_rules! await {
    ($tloop:expr => $fut:expr) => {
        $tloop.block_on($fut).unwrap()
    };
}

pub fn run_parent(mut tokio_loop: TokioLoop, channel: MessageChannel<Message, Message>) {
    let channel = await!(tokio_loop => channel.send(Message::Hello));
    println!("parent sent hello");
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::Ehlo) = message {} else {
        panic!("expected Ehlo, got {:?}", message);
    }
    println!("parent received ehlo");

    // Use child to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file.txt");
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&file_path).unwrap();
    write!(file, "{}", STRING_WRITTEN_TO_FILE).unwrap();
    let channel = await!(tokio_loop => channel.send(Message::ReadAFile(file.into())));
    println!("parent sent file handle");
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::FileSaidThis(data)) = message {
        println!("parent received file contents");
        assert_eq!(STRING_WRITTEN_TO_FILE, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    // Read a file for child
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    let channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        println!("parent received file handle");
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        await!(tokio_loop => channel.send(Message::FileSaidThis(data)))
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };
    println!("parent sent file contents");

    // Share a socket
    let tcp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp_listener.local_addr().unwrap().port();
    let client_socket_thread = ::std::thread::spawn(move || {
        TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap()
    });
    let (server_socket, _) = tcp_listener.accept().unwrap();
    let mut client_socket = client_socket_thread.join().unwrap();
    let channel = await!(tokio_loop => channel.send(Message::HaveASocket(SendableSocket(server_socket))));
    println!("parent sent socket");
    client_socket.write("hello".as_bytes()).unwrap();

    // Create a shared mutex
    let memory = SharedMem::new(4096).unwrap();
    let memory_map = memory.map(.., SharedMemAccess::ReadWrite).unwrap();
    let shared_mem = unsafe {
        &*(memory_map.pointer().offset(MUTEX_SHM_SIZE as isize) as *const AtomicBool)
    };
    let mutex = unsafe { IpcMutex::new_with_memory(memory_map, 0) }.unwrap();
    // Interact with shared mutex
    shared_mem.store(false, Ordering::SeqCst);
    let guard = mutex.lock();
    let channel = await!(tokio_loop => channel.send(Message::HaveAMutex(memory.clone_with_access(SharedMemAccess::ReadWrite).unwrap(), mutex.handle().unwrap())));
    println!("parent sent mutex");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(false, shared_mem.load(Ordering::SeqCst));
    mem::drop(guard);
    let (message, _channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::WroteToSharedMem) = message {
        println!("received shared memory write ack");
        assert_eq!(true, shared_mem.load(Ordering::SeqCst));
    } else {
        panic!("expected WroteToSharedMem, got {:?}", message);
    }
}

pub fn run_child(mut tokio_loop: TokioLoop, channel: MessageChannel<Message, Message>) {
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::Hello) = message { } else {
        panic!("expected Hello, got {:?}", message);
    }
    println!("child received hello");
    let channel = await!(tokio_loop => channel.send(Message::Ehlo));
    println!("child sent ehlo");

    // Read a file for parent
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    let channel = if let Some(Message::ReadAFile(SendableFile(mut file))) = message {
        println!("child received file handle");
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        await!(tokio_loop => channel.send(Message::FileSaidThis(data)))
    } else {
        panic!("expected ReadAFile, got {:?}", message);
    };
    println!("child sent file contents");

    // Use parent to read a file
    let file_path = env::temp_dir().join("rust_sandbox_ipc_test_file_two.txt");
    let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&file_path).unwrap();
    write!(file, "{}", STRING_WRITTEN_TO_FILE2).unwrap();
    let channel = await!(tokio_loop => channel.send(Message::ReadAFile(file.into())));
    println!("child sent file handle");
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::FileSaidThis(data)) = message {
        println!("child received file contents");
        assert_eq!(STRING_WRITTEN_TO_FILE2, data);
    } else {
        panic!("expected FileSaidThis, got {:?}", message);
    }

    // Receive a socket
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    if let Some(Message::HaveASocket(SendableSocket(mut server_socket))) = message {
        let mut buffer = vec![0u8; 256];
        let bytes_read = server_socket.read(&mut buffer).unwrap();
        assert_eq!("hello", ::std::str::from_utf8(&buffer[..bytes_read]).unwrap());
    } else {
        panic!("expected HaveASocket, got {:?}", message);
    }

    // Interact with a mutex from the parent
    let (message, channel) = await!(tokio_loop => channel.into_future().map_err(|(err, _)| err));
    let _channel = if let Some(Message::HaveAMutex(memory, mutex)) = message {
        println!("child received mutex");
        let memory_map = memory.map(.., SharedMemAccess::ReadWrite).unwrap();
        let mutex = unsafe { IpcMutex::from_handle(mutex, memory_map.clone()).unwrap() };
        let shared_mem = unsafe {
            &*(memory_map.pointer().offset(MUTEX_SHM_SIZE as isize) as *const AtomicBool)
        };

        let _guard = mutex.lock();
        shared_mem.store(true, Ordering::SeqCst);
        await!(tokio_loop => channel.send(Message::WroteToSharedMem))
    } else {
        panic!("expected HaveAMutex, got {:?}", message);
    };
    println!("child send shared memory write ack");
}