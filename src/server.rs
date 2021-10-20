//! Contains types and traits to use on authoritative server.
//!
//! ## Usage
//!
//! ```
//! # use {evoke::server::{ServerSystem, DummyRemotePlayer}, core::marker::PhantomData};
//! # async fn server<'a>(world: &'a mut hecs::World, scope: &'a scoped_arena::Scope<'a>) -> eyre::Result<()> {
//! let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 12345)).await?;
//!
//! #[derive(Clone, PartialEq, serde::Serialize)]
//! struct Foo;
//!
//! let mut server = ServerSystem::builder()
//!     .with_descriptor::<Foo>() // Server will replicate `Foo` component
//!     .with_player::<DummyRemotePlayer>()
//!     .build(listener);
//!
//! server.run(world, scope).await?;
//! # Ok(())
//! # }
//! ```

use std::{
    any::{type_name, Any, TypeId},
    collections::{HashMap, VecDeque},
    convert::TryFrom,
    future::Future,
    marker::PhantomData,
    num::NonZeroU64,
    pin::Pin,
};

use alkahest::{Bytes, FixedUsize, Pack};

use bincode::Options as _;
use bitsetium::{BitEmpty, BitSet, BitTestNone};
use evoke_core::{
    channel::tcp::TcpChannel,
    client_server::{ClientId, Event, PlayerId, ServerSession},
};
use hecs::{Component, Entity, Fetch, Query, World};
use scoped_arena::Scope;
use tokio::net::TcpListener;
use tracing::instrument;

use super::{
    bincode_opts,
    nid::{EntityHeader, EntityMapper, IdGen, NetId},
    InputSchema, WorldPacked, WorldSchema,
};

/// This component is a marker for `ServerSystem`.
/// `ServerSystem` replicates only entities marked with this component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerOwned;

/// Shortcut for item type fetched by descriptor's query.
pub type DescriptorFetchItem<'a, T> =
    <<<T as Descriptor>::Query as Query>::Fetch as Fetch<'a>>::Item;

/// Shortcut for packed type produced by descriptor.
pub type DescriptorPackType<'a, T> = <T as DescriptorPack<'a>>::Pack;

/// Result of `Descriptor::replicate`.
/// Either signals that state wasn't modified and need not to be send,
/// or contains serializable representation.
pub enum Replicate<T> {
    Unmodified,
    Modified(T),
}

/// This trait is conceptually part of `Descriptor` trait.
/// Used as a workaround for HKT.
pub trait DescriptorPack<'a> {
    /// Data ready to be packed.
    type Pack: serde::Serialize + 'a;
}

/// This trait manages components replication.
/// Typical implementation covers only one component, but is not limited to do so.
///
/// Component type itself may implement this trait, see [this blanket impl].
/// To override behavior custom descriptor type can be used.
///
/// Here are some of possible cases where custom descriptor is needed or desired:
/// * Components that are not implement `serde::Serialize`.
/// * Components that may be serialized to smaller representation.
/// * Cover multiple related components at once.
/// * Approximate comparison with `History` to send updates less frequently.
///
/// [this blanket impl]: trait.Descriptor.html#impl-Descriptor
pub trait Descriptor: for<'a> DescriptorPack<'a> + 'static {
    /// Query that will be performed during replication process.
    type Query: Query;

    /// Component containing "history" of the components processed by this descriptor.
    /// Used by server for diff-compression.
    type History: Component;

    /// Makes history point from current state.
    fn history(item: DescriptorFetchItem<'_, Self>) -> Self::History;

    /// Replicates state.
    /// Returns serializable data or signals that state is unmodified and should be skipped.
    fn replicate<'a>(
        item: DescriptorFetchItem<'a, Self>,
        history: Option<&Self::History>,
        scope: &'a Scope<'_>,
    ) -> Replicate<DescriptorPackType<'a, Self>>;
}

impl<'a, T> DescriptorPack<'a> for T
where
    T: Component + serde::Serialize + Clone + PartialEq,
{
    type Pack = &'a T;
}

impl<T> Descriptor for T
where
    T: Component + serde::Serialize + Clone + PartialEq,
{
    type Query = &'static T;
    type History = T;

    fn history(item: &T) -> T {
        item.clone()
    }

    fn replicate<'a>(item: &'a T, history: Option<&T>, _scope: &'a Scope<'_>) -> Replicate<&'a T> {
        match history {
            Some(history) if *history == *item => Replicate::Unmodified,
            _ => Replicate::Modified(item),
        }
    }
}

struct PlayerIdDescriptor {}

impl DescriptorPack<'_> for PlayerIdDescriptor {
    type Pack = NonZeroU64;
}

impl Descriptor for PlayerIdDescriptor {
    type Query = &'static PlayerId;
    type History = PlayerId;

    fn history(item: &PlayerId) -> Self::History {
        *item
    }

    fn replicate<'a>(
        item: &'a PlayerId,
        history: Option<&PlayerId>,
        _scope: &'a Scope<'_>,
    ) -> Replicate<NonZeroU64> {
        match history {
            Some(history) if *history == *item => Replicate::Unmodified,
            _ => Replicate::Modified(item.0),
        }
    }
}

#[repr(transparent)]
struct History<T> {
    buf: VecDeque<Option<T>>,
}

/// Missing history entry.
struct Missing;

/// Result of history tracking.
#[derive(Debug)]
enum TrackReplicate<T> {
    Unmodified,
    Modified(T),
    Removed,
}

impl<T> TrackReplicate<T> {
    fn write_mask<B>(&self, idx: usize, mask: &mut B)
    where
        B: BitSet,
    {
        match self {
            TrackReplicate::Unmodified => {}
            TrackReplicate::Modified(_) => {
                mask.set(idx * 2);
                mask.set(idx * 2 + 1);
            }
            TrackReplicate::Removed => {
                mask.set(idx * 2);
            }
        }
    }
}

impl<T> History<T> {
    fn new() -> Self {
        History {
            buf: VecDeque::with_capacity(8),
        }
    }

    fn track_replicate<'a, D>(
        &self,
        back: u64,
        item: Option<DescriptorFetchItem<'a, D>>,
        scope: &'a Scope<'_>,
    ) -> TrackReplicate<DescriptorPackType<'a, D>>
    where
        D: Descriptor<History = T>,
    {
        let old = self.fetch(back);

        let replicate = match (item, old) {
            (None, Ok(None)) => return TrackReplicate::Unmodified,
            (None, Ok(Some(_)) | Err(_)) => {
                return TrackReplicate::Removed;
            }
            (Some(item), Err(_)) => D::replicate(item, None, scope),
            (Some(item), Ok(old)) => D::replicate(item, old, scope),
        };

        match replicate {
            Replicate::Unmodified => TrackReplicate::Unmodified,
            Replicate::Modified(pack) => TrackReplicate::Modified(pack),
        }
    }

    fn fetch(&self, back: u64) -> Result<Option<&T>, Missing> {
        match usize::try_from(back) {
            Err(_) => Err(Missing),
            Ok(back) => {
                if self.buf.len() <= back {
                    Err(Missing)
                } else {
                    Ok(self.buf[back].as_ref())
                }
            }
        }
    }

    fn add(&mut self, value: Option<T>) {
        self.buf.push_front(value)
    }
}

struct ServerEntity<T> {
    nid: NetId,
    history: T,
}

#[derive(Clone, Copy)]
struct WorldReplicaPack<'a, T, B> {
    world: &'a World,
    scope: &'a Scope<'a>,
    mapper: &'a EntityMapper,
    back: u64,
    marker: PhantomData<fn(T, B)>,
}

impl<B, R> Pack<WorldSchema> for WorldReplicaPack<'_, R, B>
where
    B: BitEmpty + BitTestNone + BitSet + serde::Serialize,
    R: Replicator,
{
    fn pack(self, offset: usize, output: &mut [u8]) -> (WorldPacked, usize) {
        <R as Replicator>::pack::<B>(
            self.world,
            self.scope,
            self.mapper,
            self.back,
            offset,
            output,
        )
    }
}

trait Replicator {
    fn init_entities(
        world: &mut World,
        id_gen: &mut IdGen,
        mapper: &mut EntityMapper,
        scope: &Scope<'_>,
    );

    fn update_history(world: &mut World);

    fn pack<BITSET>(
        world: &World,
        scope: &Scope<'_>,
        mapper: &EntityMapper,
        back: u64,
        offset: usize,
        output: &mut [u8],
    ) -> (WorldPacked, usize)
    where
        BITSET: BitEmpty + BitTestNone + BitSet + serde::Serialize;
}

impl Replicator for () {
    fn init_entities(
        world: &mut World,
        id_gen: &mut IdGen,
        mapper: &mut EntityMapper,
        scope: &Scope<'_>,
    ) {
        let query = world
            .query_mut::<()>()
            .with::<ServerOwned>()
            .without::<ServerEntity<(History<PlayerId>,)>>();

        let entities: &[Entity] = scope.to_scope_from_iter(query.into_iter().map(|(e, ())| e));

        for &e in entities {
            let nid = mapper.new_nid(id_gen, e);

            world
                .insert_one(
                    e,
                    ServerEntity {
                        nid,
                        history: (History::<PlayerId>::new(),),
                    },
                )
                .unwrap();
        }
    }

    fn update_history(world: &mut World) {
        let query =
            world.query_mut::<(&mut ServerEntity<(History<PlayerId>,)>, Option<&PlayerId>)>();

        for (_, (server, pid)) in query {
            #[allow(non_snake_case)]
            let (PlayerIdDescriptor,) = &mut server.history;

            PlayerIdDescriptor.add(pid.map(PlayerIdDescriptor::history));
        }
    }

    fn pack<BITSET>(
        world: &World,
        scope: &Scope<'_>,
        mapper: &EntityMapper,
        back: u64,
        offset: usize,
        output: &mut [u8],
    ) -> (WorldPacked, usize)
    where
        BITSET: BitEmpty + BitTestNone + BitSet + serde::Serialize,
    {
        let opts = bincode_opts();

        let mut query = world.query::<(&ServerEntity<(History<PlayerId>,)>, Option<&PlayerId>)>();

        let mut cursor = std::io::Cursor::new(output);

        let mut updated = 0;

        for (_, (server, pid)) in query.iter() {
            let mut header = EntityHeader {
                nid: server.nid,
                mask: BITSET::empty(),
            };

            #[allow(non_snake_case)]
            let (PlayerIdDescriptor,) = &server.history;

            // Begin for each component + player_id.

            let pid = PlayerIdDescriptor.track_replicate::<PlayerIdDescriptor>(back, pid, scope);
            pid.write_mask(0, &mut header.mask);

            // End for each component + player_id.

            if !header.mask.test_none() {
                opts.serialize_into(&mut cursor, &header)
                    .expect("State is too big");

                // Begin for each component + player_id.

                if let TrackReplicate::Modified(pid) = pid {
                    opts.serialize_into(&mut cursor, &pid)
                        .expect("State is too big");
                }

                // End for each component + player_id.

                updated += 1;
            }
        }

        let mut removed = 0;
        for nid in mapper.iter_removed(world) {
            opts.serialize_into(&mut cursor, &nid)
                .expect("State is too big");
            removed += 1;
        }

        let len = cursor.position() as usize;

        (
            WorldPacked {
                offset: offset as FixedUsize,
                updated,
                removed,
            },
            len,
        )
    }
}

/// This trait defines how players are connected and commands are processed.
pub trait RemotePlayer: Send + Sync + 'static {
    /// Player info type.
    /// It is taken from message sent when new player is added by connected client.
    type Info: serde::de::DeserializeOwned;

    /// Player input type.
    /// It is taken from input message sent with associated player id.
    /// [`RemotePlayer::apply_input`] processes this data.
    type Input: serde::de::DeserializeOwned;

    /// Verifies player info.
    ///
    /// On success spawns required entities. Attaches provided `PlayerId` to entities that must receive commands from client.
    /// Returns `Self` associated to spawned entities.
    ///
    /// On error returns a reason.
    fn accept(info: Self::Info, pid: PlayerId, world: &mut World) -> eyre::Result<Self>
    where
        Self: Sized;

    /// Optional hook to perform an action when player is removed.
    #[inline(always)]
    fn disconnected(self, world: &mut World)
    where
        Self: Sized,
    {
        let _ = world;
    }

    /// Process input sent by associated player.
    /// This function is called for each entity that receives commands from the player.
    fn apply_input(&mut self, entity: Entity, world: &mut World, pack: Self::Input);
}

/// This type is dummy implementation of [`RemotePlayer`] trait.
pub struct DummyRemotePlayer;

impl RemotePlayer for DummyRemotePlayer {
    type Input = ();
    type Info = ();
    fn accept(_info: Self::Info, _pid: PlayerId, _world: &mut World) -> eyre::Result<Self> {
        Ok(DummyRemotePlayer)
    }
    fn apply_input(&mut self, _entity: Entity, _world: &mut World, _pack: Self::Input) {}
}

/// This type implements builder-pattern to configure [`ServerSystem`].
/// Allows adding new descriptors and setting remote player type.
pub struct ServerBuilder<P, R> {
    ids: Vec<TypeId>,
    marker: PhantomData<fn(P, R)>,
}

/// Marker type signals that remote player type is not set.
/// [`ServerBuilder`] starts with this type parameter in place where [`RemotePlayer`] is expected.
pub enum NoRemotePlayerType {}

impl ServerBuilder<NoRemotePlayerType, ()> {
    /// Returns new un-configured [`ServerBuilder`].
    pub fn new() -> Self {
        ServerBuilder {
            ids: Vec::new(),
            marker: PhantomData,
        }
    }
}

impl<R> ServerBuilder<NoRemotePlayerType, R> {
    /// Sets type of remote player.
    pub fn with_player<P>(self) -> ServerBuilder<P, R>
    where
        P: RemotePlayer,
    {
        ServerBuilder {
            ids: self.ids,
            marker: PhantomData,
        }
    }
}

impl<P> ServerBuilder<P, ()> {
    /// Adds component descriptor to manage state replication.
    pub fn with_descriptor<T>(mut self) -> ServerBuilder<P, (T,)>
    where
        T: Descriptor,
    {
        let tid = TypeId::of::<T>();
        assert!(
            !self.ids.contains(&tid),
            "Duplicate descriptor '{}'",
            type_name::<T>()
        );
        self.ids.push(tid);
        ServerBuilder {
            ids: self.ids,
            marker: PhantomData,
        }
    }

    /// Builds [`ServerSystem`] configured to use provided remote player and descriptors.
    ///
    /// Takes listener for accepting connections from clients.
    pub fn build(&self, listener: TcpListener) -> ServerSystem
    where
        P: RemotePlayer,
    {
        ServerSystem::new::<P, ()>(listener)
    }
}

/// Authoritative server system.
/// It should be configured with [`ServerBuilder`] and polled using [`ServerSystem::run`] method, preferably on fixed interval.
pub struct ServerSystem {
    session: ServerSession<TcpChannel, TcpListener>,
    players: HashMap<PlayerId, ConnectedPlayer<dyn Any + Send + Sync>>,
    id_gen: IdGen,
    mapper: EntityMapper,

    run_impl: for<'a> fn(
        &'a mut ServerSession<TcpChannel, TcpListener>,
        &'a mut IdGen,
        &'a mut EntityMapper,
        &'a mut HashMap<PlayerId, ConnectedPlayer<dyn Any + Send + Sync>>,
        &'a mut World,
        &'a Scope<'_>,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + 'a>>,
}

struct ConnectedPlayer<P: ?Sized> {
    cid: ClientId,
    player: Box<P>,
}

impl ServerSystem {
    /// Returns builder to configure new [`ServerSystem`]
    pub fn builder() -> ServerBuilder<NoRemotePlayerType, ()> {
        ServerBuilder::new()
    }

    fn new<P, R>(listener: TcpListener) -> Self
    where
        P: RemotePlayer,
        R: Replicator,
    {
        ServerSystem {
            session: ServerSession::new(listener),
            players: HashMap::new(),
            id_gen: IdGen::new(),
            mapper: EntityMapper::new(),
            run_impl: run_impl::<P, R, u32>,
        }
    }

    /// Runs server system once.
    /// Accepts new clients connections.
    /// Receives and process all arrived messages from clients.
    /// Replicates state to all connected clients.
    #[instrument(skip(self, world, scope))]
    pub async fn run(&mut self, world: &mut World, scope: &Scope<'_>) -> eyre::Result<()> {
        (self.run_impl)(
            &mut self.session,
            &mut self.id_gen,
            &mut self.mapper,
            &mut self.players,
            world,
            scope,
        )
        .await
    }
}

fn run_impl<'a, P, R, B>(
    session: &'a mut ServerSession<TcpChannel, TcpListener>,
    id_gen: &'a mut IdGen,
    mapper: &'a mut EntityMapper,
    players: &'a mut HashMap<PlayerId, ConnectedPlayer<dyn Any + Send + Sync>>,
    world: &'a mut World,
    scope: &'a Scope<'_>,
) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + 'a>>
where
    P: RemotePlayer,
    R: Replicator,
    B: BitEmpty + BitTestNone + BitSet + serde::Serialize,
{
    let opts = bincode_opts();

    let current_step = session.current_step();

    Box::pin(async move {
        loop {
            let mut events = session.events::<Bytes, (NetId, InputSchema)>(scope)?;
            let first = events.next();

            if first.is_none() {
                break;
            }

            for (cid, event) in first.into_iter().chain(events) {
                match event {
                    Event::ClientConnect(event) => {
                        tracing::info!("New client connection {:?}", cid);
                        let _ = event.accept(scope).await;
                        tracing::debug!("Accepted");
                    }
                    Event::AddPlayer(event) => {
                        tracing::info!("New player @ {:?}", cid);
                        let scope = &*scope;
                        let world = &mut *world;
                        let _ = event
                            .try_accept_with::<PlayerId, _, _, eyre::Report>(
                                |info| {
                                    let pid = id_gen.gen_pid();

                                    let info = opts.deserialize(info)?;
                                    let player = P::accept(info, pid, world)?;

                                    tracing::info!("{:?}@{:?} accepted", pid, cid);

                                    players.insert(
                                        pid,
                                        ConnectedPlayer {
                                            cid,
                                            player: Box::new(player),
                                        },
                                    );

                                    Ok(pid)
                                },
                                scope,
                            )
                            .await;
                    }
                    Event::Inputs(event) => {
                        tracing::debug!(
                            "Received inputs from {:?} ({}) | {}",
                            cid,
                            event.step(),
                            current_step
                        );
                        for (pid, (nid_res, input)) in event.inputs() {
                            if let Some(player) = players.get_mut(&pid) {
                                match nid_res {
                                    Err(err) => {
                                        tracing::error!("{:?}", err)
                                    }
                                    Ok(nid) => {
                                        tracing::trace!("Received inputs from {:?}@{:?}", pid, cid);
                                        if let Some(entity) = mapper.get(nid) {
                                            match opts.deserialize_from(input) {
                                                Err(err) => {
                                                    tracing::error!(
                                                        "Invalid input from {:?}@{:?}: {}",
                                                        pid,
                                                        cid,
                                                        err
                                                    );
                                                }
                                                Ok(pack) => {
                                                    let player = player
                                                        .player
                                                        .downcast_mut::<P>()
                                                        .expect("Invalid player type");
                                                    player.apply_input(entity, world, pack);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Event::Disconnected => {
                        tracing::info!("{:?} disconnected", cid);

                        let ids = &*scope.to_scope_from_iter(
                            players
                                .iter()
                                .filter_map(|(pid, player)| (player.cid == cid).then(|| *pid)),
                        );

                        for pid in ids {
                            let player = players.remove(pid).unwrap();
                            let player =
                                player.player.downcast::<P>().expect("Invalid player type");
                            player.disconnected(world);
                        }
                    }
                }
            }
        }

        R::init_entities(world, id_gen, mapper, scope);

        session
            .advance::<WorldSchema, _, _>(
                |back| WorldReplicaPack::<R, B> {
                    world,
                    scope,
                    mapper,
                    back,
                    marker: PhantomData,
                },
                scope,
            )
            .await;

        R::update_history(world);

        mapper.clear_removed(world);

        Ok(())
    })
}

macro_rules! for_tuple {
    ($($t:ident $b:ident),+ $(,)?) => {
        impl<PLAYER $( ,$t )+> ServerBuilder<PLAYER, ($( $t, )+)> {
            /// Adds component descriptor to manage state replication.
            pub fn with_descriptor<T>(mut self) -> ServerBuilder<PLAYER, ($($t,)+ T,)>
            where
                T: Descriptor,
            {
                let tid = TypeId::of::<T>();
                assert!(
                    !self.ids.contains(&tid),
                    "Duplicate descriptor {}",
                    type_name::<T>()
                );
                self.ids.push(tid);
                ServerBuilder {
                    ids: self.ids,
                    marker: PhantomData,
                }
            }

            /// Builds [`ServerSystem`] configured to use provided remote player and descriptors.
            ///
            /// Takes listener for accepting connections from clients.
            pub fn build(&self, listener: TcpListener) -> ServerSystem
            where
                PLAYER: RemotePlayer,
                $(
                    $t: Descriptor,
                )+
            {
                ServerSystem::new::<PLAYER, ($($t,)+)>(listener)
            }
        }

        impl<$($t,)+> Replicator for ($($t,)+)
        where
            $(
                $t: Descriptor,
            )+
        {
            fn init_entities(
                world: &mut World,
                id_gen: &mut IdGen,
                mapper: &mut EntityMapper,
                scope: &Scope<'_>,
            ) {
                let query = world
                    .query_mut::<()>()
                    .with::<ServerOwned>()
                    .without::<ServerEntity<( $( History<$t::History>, )+ History<PlayerId>,)>>();

                let entities: &[Entity] = scope.to_scope_from_iter(query.into_iter().map(|(e, ())| e));

                for &e in entities {
                    let nid = mapper.new_nid(id_gen, e);

                    world.insert_one(
                        e,
                        ServerEntity {
                            nid,
                            history: ($( History::<$t::History>::new(), )+ History::<PlayerId>::new(),),
                        },
                    ).unwrap();
                }
            }

            fn update_history(world: &mut World) {
                let query =
                    world.query_mut::<(&mut ServerEntity<( $( History<$t::History>, )+ History<PlayerId>,)>, $( Option<$t::Query>, )+ Option<&PlayerId>)>();

                for (_, (server, $( $b, )+ pid)) in query {
                    #[allow(non_snake_case)]
                    let ($( $t, )+ PlayerIdDescriptor,) = &mut server.history;

                    $(
                        $t.add($b.map($t::history));
                    )+

                    PlayerIdDescriptor.add(pid.map(PlayerIdDescriptor::history));
                }
            }

            fn pack<BITSET>(
                world: &World,
                scope: &Scope<'_>,
                mapper: &EntityMapper,
                back: u64,
                offset: usize, output: &mut [u8]) -> (WorldPacked, usize)
            where
                BITSET: BitEmpty + BitTestNone + BitSet + serde::Serialize,
            $(
                $t: Descriptor,
            )+
            {
                let opts = bincode::DefaultOptions::new().allow_trailing_bytes();

                let mut query = world.query::<(&ServerEntity<($( History<$t::History>, )+ History<PlayerId>,)>, $( Option<$t::Query>, )+ Option<&PlayerId>)>();

                let mut cursor = std::io::Cursor::new(output);

                let mut updated = 0;
                for (_, (server, $( $b, )+ pid)) in query.iter() {
                    let mut header = EntityHeader {
                        nid: server.nid,
                        mask: BITSET::empty(),
                    };
                    let mut mask_idx = 0;

                    #[allow(non_snake_case)]
                    let ($( $t, )+ PlayerIdDescriptor,) = &server.history;

                    // Begin for each component + player_id.

                    $(
                        let $b: TrackReplicate<_> = $t.track_replicate::<$t>(back, $b, scope);
                        $b.write_mask(mask_idx, &mut header.mask);
                        mask_idx += 1;
                    )+

                    let pid: TrackReplicate<_> = PlayerIdDescriptor.track_replicate::<PlayerIdDescriptor>(back, pid, scope);
                    pid.write_mask(mask_idx, &mut header.mask);

                    // End for each component + player_id.

                    if !header.mask.test_none() {
                        opts.serialize_into(&mut cursor, &header).expect("State is too big");

                        // Begin for each component + player_id.

                        $(
                            if let TrackReplicate::Modified($b) = $b {
                                opts.serialize_into(&mut cursor, &$b).expect("State is too big");
                            }
                        )+

                        if let TrackReplicate::Modified(pid) = pid {
                            opts.serialize_into(&mut cursor, &pid).expect("State is too big");
                        }

                        // End for each component + player_id.

                        updated += 1;
                    }
                }

                let mut removed = 0;
                for nid in mapper.iter_removed(world) {
                    opts.serialize_into(&mut cursor, &nid).expect("State is too big");
                    removed += 1;
                }

                let len = cursor.position() as usize;

                (
                    WorldPacked {
                        offset: offset as FixedUsize,
                        updated,
                        removed,
                    },
                    len,
                )
            }
        }
    };
}

for_tuple!(A a);
for_tuple!(A a, B b);
for_tuple!(A a, B b, C c);
for_tuple!(A a, B b, C c, D d);
for_tuple!(A a, B b, C c, D d, E e);
for_tuple!(A a, B b, C c, D d, E e, F f);
for_tuple!(A a, B b, C c, D d, E e, F f, G g);
for_tuple!(A a, B b, C c, D d, E e, F f, G g, H h);
for_tuple!(A a, B b, C c, D d, E e, F f, G g, H h, I i);
