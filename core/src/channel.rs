use std::{error::Error, future::Future};

use alkahest::{Pack, Schema, Unpacked};
use scoped_arena::Scope;

#[cfg(feature = "tcp")]
pub mod tcp;

pub trait ChannelError {
    type Error: Error + 'static;
}

pub trait ChannelFuture<'a>: ChannelError {
    type Send: Future<Output = Result<(), Self::Error>>;
    type Ready: Future<Output = Result<(), Self::Error>>;
}

/// Abstract raw bytes channel that can be used by `evoke` sessions.
pub trait Channel: ChannelError + for<'a> ChannelFuture<'a> {
    /// Attempts to send packet through the channel unreliably.
    /// Packet either arrives complete or lost.
    fn send<'a, S, P>(
        &'a mut self,
        packet: P,
        scope: &'a Scope,
    ) -> <Self as ChannelFuture<'a>>::Send
    where
        S: Schema,
        P: Pack<S>;

    /// Attempts to send packet through the channel reliabliy.
    /// Packet will arrive unless channel connection is lost.
    fn send_reliable<'a, S, P>(
        &'a mut self,
        packet: P,
        scope: &'a Scope,
    ) -> <Self as ChannelFuture<'a>>::Send
    where
        S: Schema,
        P: Pack<S>;

    /// Waits until channel is ready for `recv` call.
    fn recv_ready(&mut self) -> <Self as ChannelFuture<'_>>::Ready;

    /// Attempts to receive packet from the channel.
    /// If no new packets arrived `Ok(None)` is returned.
    fn recv<'a, S>(&mut self, scope: &'a Scope) -> Result<Option<Unpacked<'a, S>>, Self::Error>
    where
        S: Schema;
}

pub trait Listener {
    type Error: Error + 'static;
    type Channel: Channel;

    fn try_accept(&mut self) -> Result<Option<Self::Channel>, Self::Error>;
}
