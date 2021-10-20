use std::{
    collections::hash_map::{Entry, HashMap},
    mem::align_of,
    num::NonZeroU64,
};

use alkahest::{Pack, Schema, SchemaUnpack};
use evoke_core::client_server::PlayerId;
use hecs::{Entity, QueryOneError, World};

/// Value that is replicated instead of `Entity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub(crate) struct NetId(pub NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Zero NetId unpacked")]
pub(crate) struct ZeroNetIdError;

impl SchemaUnpack<'_> for NetId {
    type Unpacked = Result<Self, ZeroNetIdError>;
}

impl Schema for NetId {
    type Packed = u64;

    fn align() -> usize {
        align_of::<u64>()
    }

    fn unpack<'a>(packed: u64, _input: &'a [u8]) -> Result<Self, ZeroNetIdError> {
        NonZeroU64::new(packed).map(NetId).ok_or(ZeroNetIdError)
    }
}

impl Pack<NetId> for NetId {
    fn pack(self, _offset: usize, _output: &mut [u8]) -> (u64, usize) {
        (self.0.get(), 0)
    }
}

impl Pack<NetId> for &'_ NetId {
    fn pack(self, _offset: usize, _output: &mut [u8]) -> (u64, usize) {
        (self.0.get(), 0)
    }
}

pub(crate) struct IdGen {
    next: NonZeroU64,
}

impl IdGen {
    pub fn new() -> Self {
        IdGen {
            next: NonZeroU64::new(1).unwrap(),
        }
    }

    pub fn gen_nid(&mut self) -> NetId {
        NetId(self.gen())
    }

    pub fn gen_pid(&mut self) -> PlayerId {
        PlayerId(self.gen())
    }

    pub fn gen(&mut self) -> NonZeroU64 {
        let id = self.next;
        let next = self
            .next
            .get()
            .checked_add(1)
            .expect("u64 increment overflow");

        self.next = NonZeroU64::new(next).unwrap();

        id
    }
}

pub(crate) struct EntityMapper {
    entity_by_id: HashMap<NetId, Entity>,
}

#[cfg(any(feature = "server", feature = "client"))]
impl EntityMapper {
    #[inline(always)]
    pub fn new() -> Self {
        EntityMapper {
            entity_by_id: HashMap::new(),
        }
    }

    #[inline(always)]
    pub fn get(&self, nid: NetId) -> Option<Entity> {
        self.entity_by_id.get(&nid).copied()
    }

    #[cfg(feature = "client")]
    #[inline]
    pub fn get_or_spawn(&mut self, world: &mut World, nid: NetId) -> Entity {
        match self.entity_by_id.entry(nid) {
            Entry::Occupied(mut entry) => {
                let entity = *entry.get();

                match world.query_one_mut::<&NetId>(entity) {
                    Ok(id) => {
                        assert_eq!(*id, nid, "NetId modified on entity");
                    }
                    Err(QueryOneError::Unsatisfied) => {
                        panic!("NetId component was removed on entity");
                    }
                    Err(QueryOneError::NoSuchEntity) => {
                        let entity = world.spawn((nid,));
                        entry.insert(entity);
                    }
                }

                entity
            }
            Entry::Vacant(entry) => {
                let entity = world.spawn((nid,));
                entry.insert(entity);
                entity
            }
        }
    }

    #[cfg(feature = "server")]
    #[inline(always)]
    pub(super) fn new_nid(&mut self, gen: &mut IdGen, entity: Entity) -> NetId {
        let nid = gen.gen_nid();
        let old = self.entity_by_id.insert(nid, entity);
        debug_assert!(old.is_none(), "Non-unique NetId mapped");
        nid
    }

    #[cfg(feature = "server")]
    #[inline(always)]
    pub(super) fn iter_removed<'a>(&'a self, world: &'a World) -> impl Iterator<Item = NetId> + 'a {
        self.entity_by_id
            .iter()
            .filter_map(move |(nid, e)| (!world.contains(*e)).then(|| *nid))
    }

    #[cfg(feature = "server")]
    #[inline(always)]
    pub(super) fn clear_removed<'a>(&'a mut self, world: &'a World) {
        self.entity_by_id.retain(|_, e| world.contains(*e))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct EntityHeader<B> {
    pub nid: NetId,
    pub mask: B,
}
