use std::{
    alloc::Layout,
    convert::TryFrom,
    future::Future,
    io::Error,
    mem::{size_of, size_of_val},
    pin::Pin,
    ptr::copy_nonoverlapping,
    task::{Context, Poll},
};

use alkahest::{Pack, Schema, Unpacked};
use scoped_arena::Scope;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use super::{Channel, ChannelError, ChannelFuture, Listener};

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct TcpSend<'a> {
    buf: &'a [u8],
    stream: &'a mut TcpStream,
}

impl Future for TcpSend<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let me = self.get_mut();
        match me.stream.try_write(me.buf) {
            Ok(written) if written == me.buf.len() => Poll::Ready(Ok(())),
            Ok(written) => {
                me.buf = &me.buf[written..];
                me.stream.poll_write_ready(cx)
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct TcpRecvReady<'a> {
    stream: &'a mut TcpStream,
}

impl Future for TcpRecvReady<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let me = self.get_mut();
        me.stream.poll_read_ready(cx)
    }
}

struct RecvBuf {
    buf: Box<[u64]>,
}

impl RecvBuf {
    fn new() -> Self {
        RecvBuf {
            buf: vec![0u64; 16192].into_boxed_slice(),
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        let len = size_of_val(&self.buf[..]);
        let ptr = &mut self.buf[0] as *mut u64 as *mut u8;
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}

pub struct TcpChannel {
    stream: TcpStream,
    buf: RecvBuf,
    buf_len: usize,
}

impl TcpChannel {
    pub fn new(stream: TcpStream) -> Self {
        TcpChannel {
            stream,
            buf: RecvBuf::new(),
            buf_len: 0,
        }
    }

    pub async fn connect(addrs: impl ToSocketAddrs) -> Result<Self, Error> {
        let stream = TcpStream::connect(addrs).await?;
        Ok(TcpChannel::new(stream))
    }
}

impl ChannelError for TcpChannel {
    type Error = std::io::Error;
}

impl<'a> ChannelFuture<'a> for TcpChannel {
    type Send = TcpSend<'a>;
    type Ready = TcpRecvReady<'a>;
}

impl Channel for TcpChannel {
    fn send<'a, S, P>(&'a mut self, packet: P, scope: &'a Scope) -> TcpSend<'a>
    where
        S: Schema,
        P: Pack<S>,
    {
        let ptr = scope.alloc(Layout::from_size_align(65536, S::align()).unwrap());

        let buf = unsafe {
            let ptr = ptr.as_ptr();
            std::ptr::write_bytes(ptr as *mut u8, 0xfe, 65536);
            &mut *ptr
        };

        let size = alkahest::write(&mut buf[size_of::<TcpHeader>()..], packet);

        if u32::try_from(size).is_err() {
            panic!("Packet is too large");
        }

        let header = &TcpHeader {
            magic: MAGIC.to_le(),
            size: (size as u32).to_le(),
        };

        unsafe {
            copy_nonoverlapping(
                header as *const _ as *const _,
                buf.as_mut_ptr(),
                size_of::<TcpHeader>(),
            );
        }

        TcpSend {
            buf: &buf[..size_of::<TcpHeader>() + size],
            stream: &mut self.stream,
        }
    }

    fn send_reliable<'a, S, P>(&'a mut self, packet: P, scope: &'a Scope) -> TcpSend<'a>
    where
        S: Schema,
        P: Pack<S>,
    {
        self.send(packet, scope)
    }

    fn recv_ready<'a>(&'a mut self) -> TcpRecvReady<'a> {
        TcpRecvReady {
            stream: &mut self.stream,
        }
    }

    fn recv<'a, S>(
        &mut self,
        scope: &'a scoped_arena::Scope,
    ) -> Result<Option<Unpacked<'a, S>>, Error>
    where
        S: Schema,
    {
        // Get header
        let header = loop {
            if self.buf_len < size_of::<TcpHeader>() {
                let result = self.stream.try_read(&mut self.buf.bytes()[self.buf_len..]);

                match result {
                    Ok(read) => {
                        if read == 0 {
                            return Err(std::io::ErrorKind::ConnectionAborted.into());
                        } else {
                            self.buf_len += read;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                    Err(err) => return Err(err),
                }

                continue;
            } else {
                let mut header = TcpHeader { magic: 0, size: 0 };

                unsafe {
                    copy_nonoverlapping(
                        self.buf.bytes().as_ptr(),
                        &mut header as *mut TcpHeader as *mut u8,
                        size_of::<TcpHeader>(),
                    );
                }

                // Check magic value
                if u32::from_le(header.magic) != MAGIC {
                    unsafe {
                        // Rotate buf
                        std::ptr::copy(
                            self.buf.bytes().as_ptr().add(1),
                            self.buf.bytes().as_mut_ptr(),
                            self.buf_len - 1,
                        );
                    }
                    continue;
                }

                break header;
            }
        };

        let size = header.size as usize;

        // Get payload
        let payload = loop {
            if self.buf_len < size_of::<TcpHeader>() + size {
                let result = self.stream.try_read(&mut self.buf.bytes()[self.buf_len..]);

                match result {
                    Ok(read) => {
                        if read == 0 {
                            return Err(std::io::ErrorKind::ConnectionAborted.into());
                        } else {
                            self.buf_len += read;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                    Err(err) => return Err(err),
                }

                continue;
            } else {
                break scope.to_scope_from_iter(
                    self.buf.bytes()[size_of::<TcpHeader>()..].iter().copied(),
                );
            }
        };

        // Move buf tail
        unsafe {
            std::ptr::copy(
                self.buf.bytes().as_ptr().add(size_of::<TcpHeader>() + size),
                self.buf.bytes().as_mut_ptr(),
                self.buf_len - size + size_of::<TcpHeader>(),
            );
        }

        self.buf_len -= size + size_of::<TcpHeader>();

        let unpacked = alkahest::read::<S>(payload);

        return Ok(Some(unpacked));
    }
}

#[repr(C)]
struct TcpHeader {
    magic: u32,
    size: u32,
}

const MAGIC: u32 = u32::from_le_bytes(*b"astr");

impl Listener for TcpListener {
    type Error = Error;
    type Channel = TcpChannel;

    fn try_accept(&mut self) -> Result<Option<TcpChannel>, Error> {
        use futures_task::noop_waker_ref;

        let mut cx = Context::from_waker(noop_waker_ref());

        match self.poll_accept(&mut cx) {
            Poll::Ready(Ok((stream, _addr))) => Ok(Some(TcpChannel::new(stream))),
            Poll::Ready(Err(err)) => Err(err),
            Poll::Pending => Ok(None),
        }
    }
}
