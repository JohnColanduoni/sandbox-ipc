use std::marker::PhantomData;
use std::io;

use serde::{Serialize, Deserialize};
use bincode;
use futures::{Stream, Sink, Poll, Async, AsyncSink, StartSend};
use tokio::{AsyncRead, AsyncWrite};

pub struct BincodeDatagram<S, T, R, W = NoopWrapper> where
    S: AsyncRead + AsyncWrite,
    T: Serialize,
    R: for<'de> Deserialize<'de>,
    W: for<'a> SerializeWrapper<'a, S>,
{
    io: S,
    rx_buffer: Vec<u8>,
    tx_buffer: Vec<u8>,
    buffered_send: Option<usize>,
    _phantom: PhantomData<(T, R, W)>,
}

pub trait SerializeWrapper<'a, S> {
    type SerializeGuard: SerializeWrapperGuard<'a> + 'a;
    type DeserializeGuard: SerializeWrapperGuard<'a> + 'a;

    fn before_serialize(io: &'a mut S) -> Self::SerializeGuard;
    fn before_deserialize(io: &'a mut S) -> Self::DeserializeGuard;
}

pub trait SerializeWrapperGuard<'a> {
    fn commit(self);
}

impl<S, T, R, W> BincodeDatagram<S, T, R, W> where
    S: AsyncRead + AsyncWrite,
    T: Serialize,
    R: for<'de> Deserialize<'de>,
    W: for<'a> SerializeWrapper<'a, S>,
{
    pub fn wrap(io: S, buffer_size: usize) -> Self {
        BincodeDatagram {
            io,
            rx_buffer: vec![0u8; buffer_size],
            tx_buffer: vec![0u8; buffer_size],
            buffered_send: None,
            _phantom: PhantomData,
        }
    }

    pub fn get_ref(&self) -> &S { &self.io }
    pub fn get_mut(&mut self) -> &mut S { &mut self.io }
}

impl<S, T, R, W> Stream for BincodeDatagram<S, T, R, W> where
    S: AsyncRead + AsyncWrite,
    T: Serialize,
    R: for<'de> Deserialize<'de>,
    W: for<'a> SerializeWrapper<'a, S>,
{
    type Item = R;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<R>, io::Error> {
        let message_size = try_nb!(self.io.read(&mut self.rx_buffer));

        if message_size == 0 {
            return Ok(Async::Ready(None));
        }

        let message;
        {
            let guard = W::before_deserialize(&mut self.io);
            message = bincode::deserialize(&self.rx_buffer[0..message_size])
                .map_err(|x| io::Error::new(io::ErrorKind::InvalidData, x))?;
            guard.commit();
        };

        Ok(Async::Ready(Some(message)))
    }
}

impl<S, T, R, W> Sink for BincodeDatagram<S, T, R, W> where
    S: AsyncRead + AsyncWrite,
    T: Serialize,
    R: for<'de> Deserialize<'de>,
    W: for<'a> SerializeWrapper<'a, S>,
{
    type SinkItem = T;
    type SinkError = io::Error;

    fn start_send(&mut self, item: T) -> StartSend<T, io::Error> {
        if self.buffered_send.is_some() {
            match self.poll_complete()? {
                Async::Ready(()) => (),
                Async::NotReady => return Ok(AsyncSink::NotReady(item)),
            }
        }

        let mut cursor = io::Cursor::new(&mut self.tx_buffer[..]);

        {
            let guard = W::before_serialize(&mut self.io);
            bincode::serialize_into(&mut cursor, &item, bincode::Infinite)
                .map_err(|x| io::Error::new(io::ErrorKind::InvalidInput, x))?;
            guard.commit();
        }
        
        self.buffered_send = Some(cursor.position() as usize);

        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        if let Some(msg_len) = self.buffered_send {
            let written_bytes = try_nb!(self.io.write(&mut self.tx_buffer[0..msg_len]));

            if written_bytes != msg_len {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"));
            }

            self.buffered_send = None;

            Ok(Async::Ready(()))
        } else {
            Ok(Async::Ready(()))
        }
    }
}

pub struct NoopWrapper;
pub struct NoopWrapperGuard;

impl<'a, S> SerializeWrapper<'a, S> for NoopWrapper {
    type SerializeGuard = NoopWrapperGuard;
    type DeserializeGuard = NoopWrapperGuard;

    fn before_serialize(_io: &'a mut S) -> NoopWrapperGuard { NoopWrapperGuard }
    fn before_deserialize(_io: &'a mut S) -> NoopWrapperGuard { NoopWrapperGuard }
}

impl<'a> SerializeWrapperGuard<'a> for NoopWrapperGuard {
    fn commit(self) {}
}