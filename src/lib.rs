//!
//! Evoke provides basic building blocks to add networking capabilities to game engines.
//!
//! Evoke supports:
//! * Client-Server model with authoritative server
//!
//! ## Client-Server
//!
//! For client-server model `evoke` automatically performs state replication with delta compression from server to client\
//! and commands replication from clients to server.
//!
//! Evoke makes no assumption of components used in the game.\
//! User needs to register [`server::Descriptor`] in server and [`client::Descriptor`] in client for components that need to be replicated.
//! There're blanket implementations for components that are comparable for equality and serializable.\
//! For [server] and for [client].
//!
//! ## Core
//!
//! Evoke's core provides very abstract client and server sessions,
//! supporting sending and receiving commands like
//! `Connect`, `AddPlayer`, `SendInput`, `Update` etc
//! with generic payload.
//! Evoke's core is available as separate crate `evoke-core` and re-exported from this crate as `evoke::core`
//!
//! Unlike the `evoke` (this one) `evoke-core` does not depends on `hecs` and can be used
//! in any game engine, even written in language other than Rust if packed into FFI-ready library.
//!
//! [server]: server/trait.Descriptor.html#impl-Descriptor
//! [client]: client/trait.Descriptor.html#impl-Descriptor

use std::mem::align_of;

use alkahest::{FixedUsize, Schema, SchemaUnpack};
pub use evoke_core::client_server::PlayerId;

pub use evoke_core as core;

#[cfg(any(feature = "server", feature = "client"))]
mod nid;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

struct WorldSchema;

#[derive(Clone, Copy)]
#[repr(C)]
struct WorldPacked {
    offset: FixedUsize,
    updated: FixedUsize,
    removed: FixedUsize,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct WorldUnpacked<'a> {
    raw: &'a [u8],
    updated: usize,
    removed: usize,
}

unsafe impl bytemuck::Zeroable for WorldPacked {}
unsafe impl bytemuck::Pod for WorldPacked {}

impl<'a> SchemaUnpack<'a> for WorldSchema {
    type Unpacked = WorldUnpacked<'a>;
}

impl Schema for WorldSchema {
    type Packed = WorldPacked;

    fn align() -> usize {
        align_of::<FixedUsize>()
    }

    fn unpack<'a>(packed: WorldPacked, input: &'a [u8]) -> WorldUnpacked<'a> {
        let offset = packed.offset as usize;
        let raw = &input[offset..];
        let updated = packed.updated as usize;
        let removed = packed.removed as usize;

        WorldUnpacked {
            raw,
            updated,
            removed,
        }
    }
}

struct InputSchema;

#[derive(Clone, Copy)]
#[repr(C)]
struct InputPacked {
    offset: FixedUsize,
    len: FixedUsize,
}

unsafe impl bytemuck::Zeroable for InputPacked {}
unsafe impl bytemuck::Pod for InputPacked {}

impl<'a> SchemaUnpack<'a> for InputSchema {
    type Unpacked = &'a [u8];
}

impl Schema for InputSchema {
    type Packed = InputPacked;

    fn align() -> usize {
        align_of::<FixedUsize>()
    }

    fn unpack<'a>(packed: InputPacked, input: &'a [u8]) -> &'a [u8] {
        let offset = packed.offset as usize;
        let len = packed.len as usize;
        &input[offset..][..len]
    }
}

#[cfg(any(feature = "server", feature = "client"))]
#[inline(always)]
fn bincode_opts(
) -> bincode::config::WithOtherTrailing<bincode::DefaultOptions, bincode::config::AllowTrailing> {
    use bincode::Options;
    bincode::DefaultOptions::new().allow_trailing_bytes()
}
