use std::{
    convert::TryFrom,
    future::Future,
    io::Error,
    mem::size_of,
    pin::Pin,
    ptr::copy_nonoverlapping,
    task::{Context, Poll},
};

use alkahest::{Pack, Schema, Unpacked};
use scoped_arena::Scope;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use super::{Channel, ChannelError, ChannelFuture, Listner};

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

pub struct TcpChannel {
    stream: TcpStream,
    buf: Box<[u8]>,
    buf_len: usize,
}

impl TcpChannel {
    pub fn new(stream: TcpStream) -> Self {
        TcpChannel {
            stream,
            buf: vec![0; 65536].into_boxed_slice(),
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
        let buf = scope.to_scope_from_iter((0..65545).map(|_| 0u8));
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
        'outer: loop {
            let result = self.stream.try_read(&mut self.buf[self.buf_len..]);

            match result {
                Ok(read) => {
                    if read == 0 {
                        return Err(std::io::ErrorKind::ConnectionAborted.into());
                    } else {
                        self.buf_len += read;

                        let header = 'inner: loop {
                            if self.buf_len < size_of::<TcpHeader>() {
                                continue 'outer;
                            } else {
                                let mut header = TcpHeader { magic: 0, size: 0 };

                                unsafe {
                                    copy_nonoverlapping(
                                        self.buf.as_ptr(),
                                        &mut header as *mut TcpHeader as *mut u8,
                                        size_of::<TcpHeader>(),
                                    );
                                }

                                if u32::from_le(header.magic) != MAGIC {
                                    unsafe {
                                        std::ptr::copy(
                                            self.buf.as_ptr().add(1),
                                            self.buf.as_mut_ptr(),
                                            self.buf_len - 1,
                                        );
                                    }
                                    continue 'inner;
                                }
                                break 'inner header;
                            }
                        };

                        let size = header.size as usize;
                        if self.buf_len >= size + size_of::<TcpHeader>() {
                            let packet = scope.to_scope_from_iter(
                                self.buf[size_of::<TcpHeader>()..].iter().copied(),
                            );

                            unsafe {
                                std::ptr::copy(
                                    self.buf.as_ptr().add(size + size_of::<TcpHeader>()),
                                    self.buf.as_mut_ptr(),
                                    self.buf_len - size + size_of::<TcpHeader>(),
                                );
                            }

                            self.buf_len -= size + size_of::<TcpHeader>();

                            let unpacked = alkahest::read::<S>(&*packet);

                            return Ok(Some(unpacked));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) => return Err(err),
            }
        }
    }
}

#[repr(C)]
struct TcpHeader {
    magic: u32,
    size: u32,
}

const MAGIC: u32 = u32::from_le_bytes(*b"astr");

impl Listner for TcpListener {
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
