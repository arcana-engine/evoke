//! Client-Server kind of sessions.

use std::{mem::align_of, num::NonZeroU64};

use alkahest::{Pack, Schema, SchemaUnpack, Seq};

mod client;
mod server;

pub use self::{client::*, server::*};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct PlayerId(pub NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Zero PlayerId unpacked")]
pub struct ZeroPlayerIdError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ClientId(pub NonZeroU64);

impl SchemaUnpack<'_> for PlayerId {
    type Unpacked = Result<Self, ZeroPlayerIdError>;
}

impl Schema for PlayerId {
    type Packed = u64;

    fn align() -> usize {
        align_of::<u64>()
    }

    fn unpack<'a>(packed: u64, _input: &'a [u8]) -> Result<Self, ZeroPlayerIdError> {
        NonZeroU64::new(packed)
            .map(PlayerId)
            .ok_or(ZeroPlayerIdError)
    }
}

impl Pack<PlayerId> for PlayerId {
    fn pack(self, _offset: usize, _output: &mut [u8]) -> (u64, usize) {
        (self.0.get(), 0)
    }
}

/// More efficient schema replacement for `Option<PlayerId>`
pub struct MaybePlayerId;

impl SchemaUnpack<'_> for MaybePlayerId {
    type Unpacked = Option<PlayerId>;
}

impl Schema for MaybePlayerId {
    type Packed = u64;

    fn align() -> usize {
        align_of::<u64>()
    }

    fn unpack<'a>(packed: u64, _input: &'a [u8]) -> Option<PlayerId> {
        NonZeroU64::new(packed).map(PlayerId)
    }
}

impl Pack<MaybePlayerId> for PlayerId {
    fn pack(self, _offset: usize, _output: &mut [u8]) -> (u64, usize) {
        (self.0.get(), 0)
    }
}

impl Pack<MaybePlayerId> for Option<PlayerId> {
    fn pack(self, _offset: usize, _output: &mut [u8]) -> (u64, usize) {
        (self.map_or(0, |pid| pid.0.get()), 0)
    }
}

#[allow(unused)]
#[derive(Schema)]
enum ClientMessage<P: Schema = (), I: Schema = ()> {
    Connect {
        token: alkahest::Str,
    },
    AddPlayer {
        player: P,
    },
    Inputs {
        step: u64,
        inputs: Seq<(PlayerId, I)>,
    },
}

#[allow(unused)]
#[derive(Schema)]
enum ServerMessage<J: Schema = (), U: Schema = ()> {
    Connected { step: u64, step_delta_ns: u64 },
    PlayerJoined { info: J },
    Updates { server_step: u64, updates: U },
}
