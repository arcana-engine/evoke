use std::{
    alloc::Layout,
    future::Future,
    net::Ipv4Addr,
    num::NonZeroU64,
    time::{Duration, Instant},
};

use alkahest::{Schema, Str};
use lloth::{
    channel::{tcp::TcpChannel, Channel, ChannelError},
    client_server::{ClientSession, Event, PlayerId, ServerError, ServerSession},
};
use scoped_arena::Scope;
use tokio::net::{TcpListener, TcpStream};

#[derive(Schema)]
struct TestJoinInfo {
    player_id: PlayerId,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let step = Duration::new(0, 20_000_000);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 12523))
        .await
        .expect("Failed to create TCP listener");

    let mut server = ServerSession::new(listener);

    let set = tokio::task::LocalSet::new();

    // Spawn Server
    set.spawn_local(async move {
        let mut scope = Scope::new();
        let mut next_step = Instant::now();
        let mut last_advance = next_step;

        loop {
            scope.reset();

            tokio::time::sleep_until(next_step.into()).await;
            next_step += step;

            for (cid, event) in server
                .events::<u32, u32>(&scope)
                .expect("Failed to poll server session events")
            {
                match event {
                    Err(ServerError::UnexpectedMessage) => {
                        panic!("Client {:?} sent invalid event", cid,);
                    }
                    Err(ServerError::ChannelError { source }) => {
                        panic!("Client {:?} channel failed: {}", cid, source);
                    }
                    Ok(Event::ClientConnect(event)) => {
                        event
                            .accept(&scope)
                            .await
                            .expect("Failed to accept new client");
                    }
                    Ok(Event::AddPlayer(event)) => {
                        println!("New player info: {}", event.player());

                        event
                            .accept(
                                TestJoinInfoPack {
                                    player_id: PlayerId(NonZeroU64::new(1).unwrap()),
                                },
                                &scope,
                            )
                            .await
                            .expect("Failed to accept new player")
                    }
                    Ok(Event::Inputs(event)) => {
                        for (player, input) in event.inputs() {
                            println!("New player {:?} input: {}", player, input);
                        }
                    }
                }
            }

            let now = Instant::now();
            let delta = (now - last_advance).as_nanos() as u64;
            last_advance = now;

            server.advance::<Str, _>(delta, "qwe", &scope).await;
        }
    });

    // Spawn Clients
    set.spawn_local(async {
        let scope = Scope::new();

        let stream = TcpChannel::connect((Ipv4Addr::LOCALHOST, 12523))
            .await
            .expect("Failed to connect to server");

        let mut client = ClientSession::new(stream, &scope)
            .await
            .expect("Failed to connect to server");

        let info = client
            .add_player::<u32, TestJoinInfo, _>(1, &scope)
            .await
            .expect("Failed to add player 1");

        let pid = info.player_id.expect("Invalid PlayerID from server");
        println!("Player {:?} registered", pid);

        for i in 0..20 {
            client
                .send_inputs::<u32, _, _>([(pid, i)], &scope)
                .await
                .expect("Failed to send inputs to server");

            if let Some(updates) = client
                .advance::<Str>(16666666, &scope)
                .expect("Failed to advance client")
            {
                println!("Updates: {} at {}", updates.updates, updates.server_step);
            } else {
                println!("No updates");
            }

            tokio::time::sleep(Duration::new(0, 16666666)).await;
        }
    });

    set.await;
}
