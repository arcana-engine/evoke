//! Contains types and traits to use on client that connects to authoritative server.
//!
//! ## Usage
//!
//! ```
//! # use {evoke::client::{ClientSystem, DummyLocalPlayer}, core::marker::PhantomData};
//! # async fn client<'a>(world: &'a mut hecs::World, scope: &'a scoped_arena::Scope<'a>) -> eyre::Result<()> {
//! #[derive(serde::Deserialize)]
//! struct Foo;
//!
//! let mut client = ClientSystem::builder()
//!     .with_descriptor::<Foo>() // Client will replicate `Foo` component from server's updates.
//!     .with_player::<DummyLocalPlayer>()
//!     .build();
//!
//! client.run(world, scope).await?;
//! # Ok(())
//! # }
//! ```

use std::{
    any::{type_name, TypeId},
    collections::HashSet,
    future::Future,
    io::Cursor,
    marker::PhantomData,
    num::NonZeroU64,
    pin::Pin,
};

use alkahest::{Bytes, FixedUsize, Pack};
use bincode::Options as _;
use bitsetium::BitTest;
use evoke_core::{
    channel::tcp::TcpChannel,
    client_server::{ClientSession, PlayerId},
};
use eyre::WrapErr as _;
use hecs::{Component, Entity, Fetch, Query, World};
use scoped_arena::Scope;
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::instrument;

use super::{
    bincode_opts,
    nid::{EntityHeader, EntityMapper, NetId},
    InputPacked, InputSchema, WorldSchema, WorldUnpacked,
};

/// Shortcut for item type fetched by descriptor's query.
pub type DescriptorFetchItem<'a, T> =
    <<<T as Descriptor>::Query as Query>::Fetch as Fetch<'a>>::Item;

/// Shortcut for packed type produced by descriptor.
pub type DescriptorPackType<T> = <T as Descriptor>::Pack;

/// This trait manages components replication.
/// Typical implementation covers only one component, but is not limited to do so.
///
/// Component type itself may implement this trait, see [this blanket impl].
/// To override behavior custom descriptor type can be used.
///
/// Here are some of possible cases where custom descriptor is needed or desired:
/// * Components that are not implement `serde::Deserialize`.
/// * Components that may be serialized to smaller representation.
/// * Cover multiple related components at once.
/// * Custom logic for inserting, modifying and removing components.
///
/// [this blanket impl]: trait.Descriptor.html#impl-Descriptor
pub trait Descriptor: 'static {
    /// Query for components.
    type Query: Query;

    /// Data ready to be unpacked.
    type Pack: serde::de::DeserializeOwned;

    /// Insert components managed by this descriptor.
    /// This method is called during replication if descriptor's query cannot be satisfied.
    fn insert(pack: Self::Pack, entity: Entity, world: &mut World);

    /// Modify components managed by this descriptor.
    /// This method is called during replication if descriptor's query is satisfied.
    fn modify(pack: Self::Pack, item: DescriptorFetchItem<'_, Self>);

    /// Remove components managed by this descriptor.
    /// This method is called during replication if descriptor's query is satisfied.
    fn remove(entity: Entity, world: &mut World);
}

impl<T> Descriptor for T
where
    T: Component + serde::de::DeserializeOwned,
{
    type Query = &'static mut T;
    type Pack = T;

    fn modify(pack: T, item: &mut T) {
        *item = pack;
    }

    fn insert(pack: T, entity: Entity, world: &mut World) {
        world.insert_one(entity, pack).unwrap();
    }

    fn remove(entity: Entity, world: &mut World) {
        let _ = world.remove_one::<T>(entity);
    }
}

struct PlayerIdDescriptor {}

impl Descriptor for PlayerIdDescriptor {
    type Query = &'static mut PlayerId;
    type Pack = NonZeroU64;

    fn modify(pack: NonZeroU64, item: &mut PlayerId) {
        item.0 = pack;
    }
    fn insert(pack: NonZeroU64, entity: Entity, world: &mut World) {
        world.insert_one(entity, PlayerId(pack)).unwrap();
    }
    fn remove(entity: Entity, world: &mut World) {
        world.remove_one::<PlayerId>(entity).unwrap();
    }
}

trait Replicator {
    fn replicate<B>(
        unpacked: WorldUnpacked<'_>,
        mapper: &mut EntityMapper,
        world: &mut World,
        scope: &Scope<'_>,
    ) -> Result<(), BadMessage>
    where
        B: BitTest + serde::de::DeserializeOwned;
}

/// Error that is returned if parsing server message failed.
/// Client does not disconnects automatically from server that send bad messages.
/// Bad message may still be partially parsed and processed, causing state to be partially replicated.
#[derive(Debug, thiserror::Error)]
pub enum BadMessage {
    #[error("Invalid entity mask")]
    InvalidMask,

    #[error("Invalid bincode")]
    InvalidBincode,
}

enum TrackReplicate {
    Unmodified,
    Modified,
    Removed,
}

impl TrackReplicate {
    fn from_mask<'a, B>(idx: usize, mask: &B) -> Result<Self, BadMessage>
    where
        B: BitTest,
    {
        match (mask.test(idx * 2), mask.test(idx * 2 + 1)) {
            (false, false) => Ok(TrackReplicate::Unmodified),
            (true, true) => Ok(TrackReplicate::Modified),
            (true, false) => Ok(TrackReplicate::Removed),
            (false, true) => Err(BadMessage::InvalidMask),
        }
    }
}

fn replicate_one<'a, T>(
    track: TrackReplicate,
    entity: Entity,
    world: &mut World,
    cursor: &'a mut Cursor<&[u8]>,
) -> Result<(), BadMessage>
where
    T: Descriptor,
{
    let opts = bincode_opts();

    let item = match world.query_one_mut::<T::Query>(entity) {
        Ok(item) => Some(item),
        Err(hecs::QueryOneError::Unsatisfied) => None,
        Err(hecs::QueryOneError::NoSuchEntity) => {
            unreachable!("EntityMapper must guarantee that entity is alive")
        }
    };
    match (item.is_some(), track) {
        (false, TrackReplicate::Removed | TrackReplicate::Unmodified) => {
            drop(item);
        }
        (true, TrackReplicate::Unmodified) => {
            drop(item);
        }
        (true, TrackReplicate::Removed) => {
            drop(item);
            T::remove(entity, world);
        }
        (false, TrackReplicate::Modified) => {
            drop(item);
            let pack = opts.deserialize_from(cursor).map_err(|err| {
                tracing::error!("Error deserializing bincode: {}", err);
                BadMessage::InvalidBincode
            })?;
            T::insert(pack, entity, world);
        }
        (true, TrackReplicate::Modified) => {
            let pack = opts.deserialize_from(cursor).map_err(|err| {
                tracing::error!("Error deserializing bincode: {}", err);
                BadMessage::InvalidBincode
            })?;
            T::modify(pack, item.unwrap())
        }
    }
    Ok(())
}

impl Replicator for () {
    fn replicate<B>(
        unpacked: WorldUnpacked<'_>,
        mapper: &mut EntityMapper,
        world: &mut World,
        _scope: &Scope<'_>,
    ) -> Result<(), BadMessage>
    where
        B: BitTest + serde::de::DeserializeOwned,
    {
        let opts = bincode_opts();

        let mut cursor = Cursor::new(unpacked.raw);

        for _ in 0..unpacked.updated {
            let header: EntityHeader<B> = opts.deserialize_from(&mut cursor).map_err(|err| {
                tracing::error!("Error deserializing bincode: {}", err);
                BadMessage::InvalidBincode
            })?;

            let entity = mapper.get_or_spawn(world, header.nid);

            // Begin for each component + player_id.

            let pid = TrackReplicate::from_mask(0, &header.mask)?;
            replicate_one::<PlayerIdDescriptor>(pid, entity, world, &mut cursor)?;

            // End for each component + player_id.
        }

        for _ in 0..unpacked.removed {
            let nid: NetId = opts.deserialize_from(&mut cursor).map_err(|err| {
                tracing::error!("Error deserializing bincode: {}", err);
                BadMessage::InvalidBincode
            })?;

            if let Some(entity) = mapper.get(nid) {
                let _ = world.despawn(entity);
            }
        }

        Ok(())
    }
}

/// Shortcut for item type fetched by local player's query.
pub type LocalPlayerFetchItem<'a, T> =
    <<<T as LocalPlayer>::Query as Query>::Fetch as Fetch<'a>>::Item;

/// Shortcut for packed type produced by local player.
pub type LocalPlayerPackType<'a, T> = <T as LocalPlayerPack<'a>>::Pack;

/// This trait is conceptually part of `LocalPlayer` trait.
/// Used as a workaround for HKT.
pub trait LocalPlayerPack<'a> {
    type Pack: serde::Serialize + 'a;
}

/// This trait manages commands replication.
/// Typical implementation uses single component, but is not limited to do so.
pub trait LocalPlayer: for<'a> LocalPlayerPack<'a> + 'static {
    /// Query that will be performed during replication process.
    type Query: Query;

    /// Replicates state.
    /// Returns serializable data containing all commands for an entity.
    fn replicate<'a>(
        item: <<Self::Query as Query>::Fetch as Fetch<'a>>::Item,
        scope: &'a Scope<'_>,
    ) -> LocalPlayerPackType<'a, Self>;
}

/// This type is dummy implementation of [`LocalPlayer`] trait.
pub enum DummyLocalPlayer {}

impl LocalPlayerPack<'_> for DummyLocalPlayer {
    type Pack = ();
}

impl LocalPlayer for DummyLocalPlayer {
    type Query = ();

    fn replicate<'a>(
        item: <<Self::Query as Query>::Fetch as Fetch<'a>>::Item,
        _scope: &'a Scope<'_>,
    ) -> LocalPlayerPackType<'a, Self> {
        item
    }
}

/// This type implements builder-pattern to configure [`ClientBuilder`].
/// Allows adding new descriptors and setting local player type.
pub struct ClientBuilder<I, R> {
    ids: Vec<TypeId>,
    marker: PhantomData<fn(I, R)>,
}

/// Marker type signals that remote player type is not set.
/// [`ClientBuilder`] starts with this type parameter in place where [`LocalPlayer`] is expected.
pub enum NoLocalPlayerType {}

impl ClientBuilder<NoLocalPlayerType, ()> {
    /// Returns new un-configured [`ClientBuilder`].
    pub fn new() -> Self {
        ClientBuilder {
            ids: Vec::new(),
            marker: PhantomData,
        }
    }
}

impl<R> ClientBuilder<NoLocalPlayerType, R> {
    /// Sets type of local player.
    pub fn with_player<P>(self) -> ClientBuilder<P, R>
    where
        P: LocalPlayer,
    {
        ClientBuilder {
            ids: self.ids,
            marker: PhantomData,
        }
    }
}

impl<P> ClientBuilder<P, ()> {
    /// Adds component descriptor to manage state replication.
    pub fn with_descriptor<T>(mut self) -> ClientBuilder<P, (T,)>
    where
        T: Descriptor,
    {
        let tid = TypeId::of::<T>();
        assert!(
            !self.ids.contains(&tid),
            "Duplicate replica descriptor '{}'",
            type_name::<T>()
        );
        self.ids.push(tid);
        ClientBuilder {
            ids: self.ids,
            marker: PhantomData,
        }
    }

    /// Builds [`ClientSystem`] configured to use provided local player and descriptors.
    ///
    /// Takes listener for accepting connections from clients.
    pub fn build(&self) -> ClientSystem
    where
        P: LocalPlayer,
    {
        ClientSystem::new::<P, ()>()
    }
}

/// Client system that connects to authoritative server.
/// It should be configured with [`ClientBuilder`] and polled using [`ClientSystem::run`] method.
pub struct ClientSystem {
    session: Option<ClientSession<TcpChannel>>,
    mapper: EntityMapper,
    controlled: HashSet<PlayerId>,
    send_inputs: for<'a> fn(
        &'a HashSet<PlayerId>,
        &'a mut ClientSession<TcpChannel>,
        &'a mut World,
        &'a Scope<'_>,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + 'a>>,
    replicate: fn(
        &mut ClientSession<TcpChannel>,
        &mut EntityMapper,
        &mut World,
        scope: &Scope<'_>,
    ) -> eyre::Result<()>,
}

impl ClientSystem {
    /// Returns builder to configure new [`ClientSystem`]
    pub fn builder() -> ClientBuilder<NoLocalPlayerType, ()> {
        ClientBuilder::new()
    }

    fn new<P, R>() -> Self
    where
        P: LocalPlayer,
        R: Replicator,
    {
        ClientSystem {
            session: None,
            mapper: EntityMapper::new(),
            controlled: HashSet::new(),
            send_inputs: send_inputs::<P>,
            replicate: replicate::<R, u32>,
        }
    }

    /// Connects to server at specified address.
    /// If connection succeeds, disconnects to previously connected server if any.
    pub async fn connect(
        &mut self,
        addr: impl ToSocketAddrs,
        scope: &Scope<'_>,
    ) -> eyre::Result<()> {
        let stream = TcpStream::connect(addr).await?;
        self.connect_with_stream(stream, scope).await
    }

    /// Connects to server using provided [`TcpStream`].
    /// If connection succeeds, disconnects to previously connected server if any.
    pub async fn connect_with_stream(
        &mut self,
        stream: TcpStream,
        scope: &Scope<'_>,
    ) -> eyre::Result<()> {
        let session = ClientSession::new(TcpChannel::new(stream), scope).await?;

        if let Some(session) = self.session.take() {
            drop(session);
        }

        self.session = Some(session);

        Ok(())
    }

    /// Adds new player.
    /// On success returns `PlayerId` of newly added player.
    /// `player` argument must match schema expected by server's [`RemotePlayer::Info`].
    #[cfg_attr(feature = "server", doc = "")]
    #[cfg_attr(
        feature = "server",
        doc = "[`RemotePlayer::Info`]: crate::server::RemotePlayer"
    )]
    pub async fn add_player(
        &mut self,
        player: &impl serde::Serialize,
        scope: &Scope<'_>,
    ) -> eyre::Result<PlayerId> {
        let opts = bincode::DefaultOptions::new().allow_trailing_bytes();
        let player = opts.serialize(player)?;

        let id = self
            .session
            .as_mut()
            .expect("Attempt to add player in disconnected ClientSystem")
            .add_player::<Bytes, PlayerId, _>(player, scope)
            .await
            .map_or_else(
                |err| Err(eyre::Report::from(err)),
                |res| res.map_err(eyre::Report::from),
            )
            .wrap_err_with(|| eyre::eyre!("Failed to add player"))?;

        let no_collision = self.controlled.insert(id);
        if !no_collision {
            return Err(eyre::eyre!("PlayerId({:?}) collision detected", id));
        }
        Ok(id)
    }

    /// Runs client system once.
    /// Sends commands from all entities controlled by one of this clients players.
    /// Receives and processes state updates.
    #[instrument(skip(self, world, scope))]
    pub async fn run<'a>(
        &mut self,
        world: &'a mut World,
        scope: &'a Scope<'_>,
    ) -> eyre::Result<()> {
        if let Some(session) = &mut self.session {
            (self.send_inputs)(&self.controlled, session, world, scope).await?;
            (self.replicate)(session, &mut self.mapper, world, scope)?;
        }
        Ok(())
    }
}

struct InputPack<I> {
    input: I,
}

impl<'a, I> Pack<InputSchema> for &'_ InputPack<I>
where
    I: serde::Serialize,
{
    fn pack(self, offset: usize, output: &mut [u8]) -> (InputPacked, usize) {
        let opts = bincode::DefaultOptions::new().allow_trailing_bytes();
        let mut cursor = Cursor::new(output);
        opts.serialize_into(&mut cursor, &self.input).unwrap();
        let len = cursor.position() as usize;

        (
            InputPacked {
                offset: offset as FixedUsize,
                len: len as FixedUsize,
            },
            len,
        )
    }
}

fn send_inputs<'a, P>(
    controlled: &'a HashSet<PlayerId>,
    session: &'a mut ClientSession<TcpChannel>,
    world: &'a mut World,
    scope: &'a Scope<'_>,
) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + 'a>>
where
    P: LocalPlayer,
{
    let inputs = world
        .query_mut::<(&PlayerId, &NetId, P::Query)>()
        .into_iter()
        .filter_map(|(_, (pid, nid, input))| {
            controlled.contains(pid).then(move || {
                (
                    *pid,
                    (
                        *nid,
                        InputPack {
                            input: P::replicate(input, scope),
                        },
                    ),
                )
            })
        });

    let inputs = scope.to_scope_from_iter(inputs);
    let inputs = inputs
        .iter()
        .map(|(pid, (nid, input))| ((*pid, (*nid, input))));

    tracing::debug!("Sending input ({})", session.current_step());
    Box::pin(async move {
        session
            .send_inputs::<(NetId, InputSchema), (NetId, &'a InputPack<<P as LocalPlayerPack<'a>>::Pack>), _>(inputs, scope)
            .await
            .wrap_err("Failed to send inputs from client to server")
    })
}

fn replicate<R, B>(
    session: &mut ClientSession<TcpChannel>,
    mapper: &mut EntityMapper,
    world: &mut World,
    scope: &Scope<'_>,
) -> eyre::Result<()>
where
    R: Replicator,
    B: BitTest + serde::de::DeserializeOwned,
{
    while let Some(updates) = session.advance::<WorldSchema>(scope)? {
        tracing::debug!("Received updates ({})", updates.server_step);
        R::replicate::<B>(updates.updates, mapper, world, scope)?;
    }

    Ok(())
}

macro_rules! for_tuple {
    ($($t:ident $b:ident),+ $(,)?) => {
        impl<PLAYER $( ,$t )+> ClientBuilder<PLAYER, ($( $t, )+)> {
            /// Adds component descriptor to manage state replication.
            pub fn with_descriptor<T>(mut self) -> ClientBuilder<PLAYER, ($($t,)+ T,)>
            where
                T: 'static,
            {
                let tid = TypeId::of::<T>();
                assert!(
                    !self.ids.contains(&tid),
                    "Duplicate replica descriptor {}",
                    type_name::<T>()
                );
                self.ids.push(tid);
                ClientBuilder {
                    ids: self.ids,
                    marker: PhantomData,
                }
            }

            /// Builds [`ClientSystem`] configured to use provided local player and descriptors.
            ///
            /// Takes listener for accepting connections from clients.
            pub fn build(&self) -> ClientSystem
            where
                PLAYER: LocalPlayer,
                $(
                    $t: Descriptor,
                )+
            {
                ClientSystem::new::<PLAYER, ($($t,)+)>()
            }
        }

        impl<$( $t, )+> Replicator for ($( $t, )+)
        where
            $(
                $t: Descriptor,
            )+
        {
            fn replicate<BITSET>(
                unpacked: WorldUnpacked<'_>,
                mapper: &mut EntityMapper,
                world: &mut World,
                _scope: &Scope<'_>,
            ) -> Result<(), BadMessage>
            where
                BITSET: BitTest + serde::de::DeserializeOwned,
            {
                let opts = bincode::DefaultOptions::new().allow_trailing_bytes();

                let mut cursor = Cursor::new(unpacked.raw);

                for _ in 0..unpacked.updated {
                    // tracing::error!("REST: {:?}", &cursor.get_ref()[cursor.position() as usize..]);

                    let header: EntityHeader<BITSET> = opts
                        .deserialize_from(&mut cursor)
                        .map_err(|_| BadMessage::InvalidBincode)?;

                    // tracing::error!("NetId: {:?}", header.nid);

                    let entity = mapper.get_or_spawn(world, header.nid);

                    let mut mask_idx = 0;

                    // Begin for each component + player_id.

                    $(
                        let $b = TrackReplicate::from_mask(mask_idx, &header.mask)?;
                        replicate_one::<$t>($b, entity, world, &mut cursor)?;
                        mask_idx += 1;
                    )+

                    let pid = TrackReplicate::from_mask(mask_idx, &header.mask)?;
                    replicate_one::<PlayerIdDescriptor>(pid, entity, world, &mut cursor)?;

                    // End for each component + player_id.
                }

                for _ in 0..unpacked.removed {
                    let nid: NetId = opts.deserialize_from(&mut cursor).map_err(|err| {
                        tracing::error!("Error deserializing bincode: {}", err);
                        BadMessage::InvalidBincode
                    })?;

                    if let Some(entity) = EntityMapper::get(mapper, nid) {
                        let _ = world.despawn(entity);
                    }
                }

                Ok(())
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
