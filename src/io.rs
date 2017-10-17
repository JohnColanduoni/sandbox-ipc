use super::*;

use std::{io, fs, slice};
use std::ops::Deref;
use std::io::Read;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use futures::{Stream, Sink, Poll, StartSend};
use tokio::reactor::{Handle as TokioHandle};

pub struct MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    inner: BincodeDatagram<platform::MessageChannel, T, R>
}

impl<T, R> MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>
{
    pub fn pair(tokio_handle: &TokioHandle, max_message_size: usize) -> io::Result<(Self, Self)> {
        let (a, b) = platform::MessageChannel::pair(tokio_handle)?;

        Ok((
            MessageChannel { inner: BincodeDatagram::wrap(a, max_message_size) },
            MessageChannel { inner: BincodeDatagram::wrap(b, max_message_size) },
        ))
    }

    pub fn from_os(channel: platform::MessageChannel, max_message_size: usize) -> io::Result<Self> {
        Ok(MessageChannel { inner: BincodeDatagram::wrap(channel, max_message_size) })
    }
}

impl<T, R> Stream for MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>,
{
    type Item = R;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<R>, io::Error> {
        let _guard = platform::push_current_channel_deserialize(self.inner.get_ref());
        self.inner.poll()
    }
}

impl<T, R> Sink for MessageChannel<T, R> where
    T: Serialize,
    R: for<'de> Deserialize<'de>,
{
    type SinkItem = T;
    type SinkError = io::Error;

    fn start_send(&mut self, item: T) -> StartSend<T, io::Error> {
        let _guard = platform::push_current_channel_serialize(self.inner.get_ref());
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.inner.poll_complete()
    }
}

#[derive(Debug)]
pub struct SendableFile<B = fs::File>(pub B) where
    B: ::std::borrow::Borrow<fs::File>;

#[derive(Serialize, Deserialize, Debug)]
pub enum SendableDataSource {
    File(#[serde(with = "sendable_data_source_file")] fs::File),
    Memory(SharedMem),
    Inline(Vec<u8>),
}

impl From<fs::File> for SendableFile {
    fn from(file: fs::File) -> SendableFile {
        SendableFile(file)
    }
}

impl From<SendableFile> for fs::File {
    fn from(file: SendableFile) -> fs::File {
        file.0
    }
}

mod sendable_data_source_file {
    use super::*;

    pub fn serialize<S>(f: &fs::File, serializer: S) -> Result<S::Ok, S::Error> where
        S: Serializer,
    {
        SendableFile(f).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<fs::File, D::Error> where
        D: Deserializer<'de>,
    {
        Ok(SendableFile::deserialize(deserializer)?.0)
    }
}

pub struct SendableDataSourceReader(_SendableDataSourceReader);

enum _SendableDataSourceReader {
    File(fs::File),
    Memory(SharedMemMap<SharedMem>, io::Cursor<&'static [u8]>),
    Inline(io::Cursor<Vec<u8>>),
}

pub struct SendableDataSourceBytes(_SendableDataSourceBytes);

enum _SendableDataSourceBytes {
    Vec(Vec<u8>),
    Shared(SharedMemMap<SharedMem>),
}

impl SendableDataSource {
    pub fn to_read(self) -> io::Result<SendableDataSourceReader> {
        Ok(SendableDataSourceReader(match self {
            SendableDataSource::File(file) => {
                _SendableDataSourceReader::File(file)
            },
            SendableDataSource::Memory(shared_mem) => {
                unsafe {
                    let map = shared_mem.map(.., SharedMemAccess::Read)?;
                    let slice = ::std::slice::from_raw_parts(map.pointer() as _, map.len());
                    _SendableDataSourceReader::Memory(map, io::Cursor::new(slice))
                }
            },
            SendableDataSource::Inline(bytes) => {
                _SendableDataSourceReader::Inline(io::Cursor::new(bytes))
            }
        }))
    }

    pub fn to_bytes(self) -> io::Result<SendableDataSourceBytes> {
        Ok(SendableDataSourceBytes(match self {
            SendableDataSource::File(mut file) => {
                let mut buffer = Vec::new();
                file.read_to_end(&mut buffer)?;
                _SendableDataSourceBytes::Vec(buffer)
            },
            SendableDataSource::Memory(shared_mem) => {
                _SendableDataSourceBytes::Shared(shared_mem.map(.., SharedMemAccess::Read)?)
            },
            SendableDataSource::Inline(bytes) => {
                _SendableDataSourceBytes::Vec(bytes)
            }
        }))
    }
}

impl From<Vec<u8>> for SendableDataSource {
    fn from(bytes: Vec<u8>) -> Self {
        SendableDataSource::Inline(bytes)
    }
}

impl From<fs::File> for SendableDataSource {
    fn from(file: fs::File) -> Self {
        SendableDataSource::File(file)
    }
}

impl io::Read for SendableDataSourceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0 {
            _SendableDataSourceReader::File(ref mut file) => file.read(buffer),
            _SendableDataSourceReader::Memory(_, ref mut cursor) => cursor.read(buffer),
            _SendableDataSourceReader::Inline(ref mut cursor) => cursor.read(buffer),
        }
    }
}

impl Deref for SendableDataSourceBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self.0 {
            _SendableDataSourceBytes::Vec(ref bytes) => bytes,
            _SendableDataSourceBytes::Shared(ref map) => unsafe {
                slice::from_raw_parts(map.pointer(), map.len())
            },
        }
    }
}