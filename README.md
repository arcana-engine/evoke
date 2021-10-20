# evoke - easy netcode

[![crates](https://img.shields.io/crates/v/evoke.svg?style=for-the-badge&label=evoke)](https://crates.io/crates/evoke)
[![docs](https://img.shields.io/badge/docs.rs-evoke-66c2a5?style=for-the-badge&labelColor=555555&logoColor=white)](https://docs.rs/evoke)
[![actions](https://img.shields.io/github/workflow/status/zakarumych/evoke/badge/master?style=for-the-badge)](https://github.com/zakarumych/evoke/actions?query=workflow%3ARust)
[![MIT/Apache](https://img.shields.io/badge/license-MIT%2FApache-blue.svg?style=for-the-badge)](COPYING)
![loc](https://img.shields.io/tokei/lines/github/zakarumych/evoke?style=for-the-badge)

Evoke provides building blocks to add networking capabilities to game engines.

Evoke supports:
* Client-Server model with authoritative server


## Client-Server

For client-server model Evoke automatically performs state replication with delta compression from server to client\
and commands replication from clients to server.

Evoke makes no assumption of components used in the game.\
User needs to register [`server::Descriptor`] in server and [`client::Descriptor`] in client for components that need to be replicated.
There're blanket implementations for components that are comparable for equality and serializable.\
For [**server**] and for [**client**].

## Core

Evoke's core provides very abstract client and server sessions,
supporting sending and receiving commands like
`Connect`, `AddPlayer`, `SendInput`, `Update` etc
with generic payload.
Evoke's core is available as separate crate `evoke-core` and re-exported from this crate as `evoke::core`

Unlike the `evoke` crate (this one) `evoke-core` does not depends on `hecs` and can be used
in any game engine, even written in language other than Rust if packed into FFI-ready library.

## Usage

To start using Evoke simply configure and run `ServerSystem` and `ClientSystem` on server and client respectively.

#### Server
To configure `ServerSystem` provide descriptors for components replication and `RemotePlayer` implementation.

```rust
# async fn foo() -> eyre::Result<()> {
/// Information associated with player.
#[derive(serde::Deserialize)]
struct MyPlayerInfo;

/// Player input serializable representation.
#[derive(serde::Deserialize)]
struct MyPlayerInput;

/// This type drives player lifecycle and input processing.
struct MyRemotePlayer;

impl evoke::server::RemotePlayer for MyRemotePlayer {
    type Info = MyPlayerInfo;
    type Input = MyPlayerInput;

    fn accept(info: MyPlayerInfo, pid: evoke::PlayerId, world: &mut hecs::World) -> eyre::Result<Self> {
        // Decide here whether accept new player based on `info` provided.
        // `Ok` signals that player is accepted.
        // `Err` signals that player is rejected.
        Ok(MyRemotePlayer)
    }

    fn apply_input(&mut self, entity: hecs::Entity, world: &mut hecs::World, pack: MyPlayerInput) {
        // Input is associated with provided entity.
        // This code should transform input and put it where other systems would be able to consume it properly.
        // Usually it do the reverse of [`client::LocalPlayer::replicate`].
    }
}

/// Component that is own descriptor.
#[derive(Clone, Copy, PartialEq, serde::Serialize)]
pub struct MyComponent;

/// Prepare channel listener.
let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 12523)).await?;

/// Build server system.
let mut server = evoke::server::ServerSystem::builder()
    .with_descriptor::<MyComponent>()
    .with_player::<MyRemotePlayer>()
    .build(listener);

let mut world = hecs::World::new();
let scope = scoped_arena::Scope::new();

// game loop
loop {
    //
    // Game loop tick
    //

    // Run server every tick.
    server.run(&mut world, &scope);
}
# Ok(()) }
```

#### Client
To configure `ClientSystem` provide descriptors for components replication and `LocalPlayer` implementation.

```rust
# async fn foo() -> eyre::Result<()> {
/// Information associated with player.
#[derive(serde::Serialize)]
struct MyPlayerInfo;

/// Player input serializable representation.
#[derive(serde::Serialize)]
struct MyPlayerInput;

/// This type drives player lifecycle and input processing.
struct MyLocalPlayer;

impl<'a> evoke::client::LocalPlayerPack<'a> for MyLocalPlayer {
    type Pack = &'a MyPlayerInput;
}

impl evoke::client::LocalPlayer for MyLocalPlayer {
    type Query = &'static MyPlayerInput;

    fn replicate<'a>(item: &'a MyPlayerInput, _scope: &'a scoped_arena::Scope<'_>) -> &'a MyPlayerInput {
        item
    }
}

/// Component that is own descriptor.
#[derive(Clone, Copy, PartialEq, serde::Deserialize)]
pub struct MyComponent;

/// Build client system.
let mut client = evoke::client::ClientSystem::builder()
    .with_descriptor::<MyComponent>()
    .with_player::<MyLocalPlayer>()
    .build();

let mut world = hecs::World::new();
let scope = scoped_arena::Scope::new();

client.connect((std::net::Ipv4Addr::LOCALHOST, 12523), &scope).await?;


let player_id: evoke::PlayerId = client.add_player(&MyPlayerInfo, &scope).await?;
// The player can controls all entities to which server attaches same `PlayerId` as component.

// game loop
loop {
    //
    // Game loop tick
    //

    // Run server every tick.
    client.run(&mut world, &scope);
}
# Ok(()) }
```

[`server::Descriptor`]: https://docs.rs/evoke/0.1.0/evoke/server/trait.Descriptor.html
[`client::Descriptor`]: https://docs.rs/evoke/0.1.0/evoke/client/trait.Descriptor.html
[**server**]: https://docs.rs/evoke/0.1.0/evoke/server/trait.Descriptor.html#impl-Descriptor
[**client**]: https://docs.rs/evoke/0.1.0/evoke/client/trait.Descriptor.html#impl-Descriptor

## License

Licensed under either of

* Apache License, Version 2.0, ([license/APACHE](license/APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([license/MIT](license/MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
