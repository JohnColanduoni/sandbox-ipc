extern crate sandbox_ipc;
extern crate tokio_core;
extern crate futures;

#[macro_use] extern crate serde_derive;
extern crate serde_json;
extern crate bincode;

use std::{env, time};
use std::process::Command;

use sandbox_ipc::{MessageChannel, ChildMessageChannel};
use sandbox_ipc::shm::{self, SharedMem};
use tokio_core::reactor::{Core as TokioLoop};
use futures::{Stream, Sink, Future};

const CHILD_CHANNEL_ENV_VAR: &str = "CHILD_CHANNEL";

fn main() {
    if let Ok(arg) = env::var(CHILD_CHANNEL_ENV_VAR) {
        let tokio_loop = TokioLoop::new().unwrap();
        let channel_serialized: ChildMessageChannel = serde_json::from_str(&arg).unwrap();
        let channel = channel_serialized.into_channel(&tokio_loop.handle()).unwrap();

        run_child(tokio_loop, channel);
    } else {
        let tokio_loop = TokioLoop::new().unwrap();
        let (a, b) = MessageChannel::pair(&tokio_loop.handle(), 8192).unwrap();

        let mut child_command = Command::new(env::current_exe().unwrap());
        let mut child = b.send_to_child(&mut child_command, |command, child_channel| {
            command.env(CHILD_CHANNEL_ENV_VAR, serde_json::to_string(child_channel).unwrap()).spawn()
        }).unwrap();

        run_parent(tokio_loop, a);

        if !child.wait().unwrap().success() {
            panic!("child process returned failure exit code");
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum Message {
    SharedMemory(SharedMem, shm::queue::Handle)
}

const ITEM_SIZE: usize = 64;
const QUEUE_LEN: usize = 32;

const ITEMS_TO_PROCESS: usize = 10_000_000;

fn run_parent(mut tokio_loop: TokioLoop, channel: MessageChannel<Message, Message>) {
    let memory = SharedMem::new(shm::Queue::required_size(ITEM_SIZE, QUEUE_LEN)).unwrap();
    let memory_clone = memory.clone(shm::Access::ReadWrite).unwrap();
    let queue = unsafe { shm::Queue::new_with_memory(ITEM_SIZE, QUEUE_LEN, memory.map(.., shm::Access::ReadWrite).unwrap(), 0).unwrap() };
    let _channel = tokio_loop.run(channel.send(Message::SharedMemory(memory_clone, queue.handle().unwrap()))).unwrap();

    let mut max = None;
    let mut min = None;

    let mut n = 0;
    let mut mean = 0f64;
    let mut m2 = 0f64;

    for _ in 0..ITEMS_TO_PROCESS {
        let pop;
        loop {
            if let Some(guard) = queue.try_pop().unwrap() {
                pop = guard;
                break;
            }
        }

        let sent_time: time::SystemTime = bincode::deserialize(&pop).unwrap();
        let elapsed = sent_time.elapsed().unwrap();
        let elapsed_ns = elapsed.subsec_nanos();

        if max.map(|x| x < elapsed_ns).unwrap_or(true) {
            max = Some(elapsed_ns);
        }
        if min.map(|x| x > elapsed_ns).unwrap_or(true) {
            min = Some(elapsed_ns);
        }

        // Mean and variance via Welford's algorithm
        n += 1;
        let delta = elapsed_ns as f64 - mean;
        mean += delta / n as f64;
        let delta2 = elapsed_ns as f64 - mean;
        m2 += delta * delta2;

    }
    let variance = m2 / (n - 1) as f64;

    println!("latency: avg {}ns (sig2 = {}, sig = {}) min {}ns max {}ns", mean as u32, variance as u32, variance.sqrt() as u32, min.unwrap(), max.unwrap());
}

fn run_child(mut tokio_loop: TokioLoop, channel: MessageChannel<Message, Message>) {
    let (message, _channel) = tokio_loop.run(channel.into_future().map_err(|(err, _)| err)).unwrap();
    let Message::SharedMemory(memory, queue) = message.unwrap();
    let queue = shm::Queue::from_handle(queue, memory.map(.., shm::Access::ReadWrite).unwrap()).unwrap();

    for _ in 0..ITEMS_TO_PROCESS {
        let mut push;
        loop {
            if let Some(guard) = queue.try_push() {
                push = guard;
                break;
            }
        }

        let now = time::SystemTime::now();
        let mut buffer: &mut [u8] = &mut push;
        bincode::serialize_into(&mut buffer, &now, bincode::Infinite).unwrap();
    }
}
