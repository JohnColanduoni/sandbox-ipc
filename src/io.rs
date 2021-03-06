use shm::{SharedMem, SharedMemMap, Access as SharedMemAccess};

use std::{io, fs, slice};
use std::ops::Deref;
use std::io::Read;

use serde::{Serialize, Deserialize, Serializer, Deserializer};

/// Wraps a normal `std::fs::File` so it may be transmitted to other processes.
#[derive(Debug)]
pub struct SendableFile<B = fs::File>(pub B) where
    B: ::std::borrow::Borrow<fs::File>;

/// Wraps normal sockets types (e.g. `std::net::TcpStream`) so they may be transmitted to
/// other processes.
/// 
/// # Windows
/// 
/// Windows file/pipe handles or sockets that have been associated with an IOCP cannot be
/// sent to other processes. Tokio is backed by an IOCP on Windows, so Tokio sockets may
/// not be sent. Do note however that Tokio sockets allow you to `accept` standard blocking
/// sockets, which *will* be elligible for transmission to other processes, where they can
/// be added to a Tokio event loop.
#[derive(Debug)]
pub struct SendableSocket<S>(pub S);

/// A source of data can be sent to other processes over a `MessageChannel` or similar
/// mechanism. It may consist of a file handle, shared memory, or inline data.
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
        Serialize::serialize(&SendableFile(f), serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<fs::File, D::Error> where
        D: Deserializer<'de>,
    {
        let file: SendableFile = Deserialize::deserialize(deserializer)?;
        Ok(file.0)
    }
}

pub struct SendableDataSourceReader(_SendableDataSourceReader);

enum _SendableDataSourceReader {
    File(fs::File),
    Memory(SharedMemMap, io::Cursor<&'static [u8]>),
    Inline(io::Cursor<Vec<u8>>),
}

pub struct SendableDataSourceBytes(_SendableDataSourceBytes);

enum _SendableDataSourceBytes {
    Vec(Vec<u8>),
    Shared(SharedMemMap),
}

impl SendableDataSource {
    /// Converts the `SendableDataSource` into an appropriate `std::io::Read` implementation.
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

    /// Converts the `SendableDataSource` into a in-memory byte array.
    /// 
    /// If the data source is a file, this will read the entirety of it to memory at once. Use `to_read`
    /// if you need only streaming access to the data.
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

#[cfg(test)]
mod tests {
    use super::*;
    use ::MessageChannel;

    use std::{fs, env};
    use std::io::{Write, Read, Seek, SeekFrom};

    use futures::{Sink, Stream};
    use tokio::runtime::Runtime;

    #[test]
    fn send_file_same_process() {
        let mut runtime = Runtime::new().unwrap();
        let (a, b) = MessageChannel::<SendableFile, SendableFile>::pair(runtime.reactor(), 8192).unwrap();

        let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true)
            .open(env::temp_dir().join("some_test_file.txt")).unwrap();
        write!(file, "hello").unwrap();
        file.flush().unwrap();

        let _a = runtime.block_on(a.send(SendableFile(file))).unwrap();
        let (message, _b) = runtime.block_on(b.into_future()).map_err(|(err, _)| err).unwrap();
        let SendableFile(mut file) = message.unwrap();

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut buffer = String::new();
        file.read_to_string(&mut buffer).unwrap();

        assert_eq!("hello", buffer);
    }
}