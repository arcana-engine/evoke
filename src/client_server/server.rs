use std::{collections::HashMap, error::Error, num::NonZeroU64};

use alkahest::{Schema, SeqUnpacked, Unpacked};
use scoped_arena::Scope;

use crate::channel::{Channel, Listner};

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum ServerError<E: Error + 'static> {
    #[error("Client channel error: {source}")]
    ChannelError {
        #[from]
        source: E,
    },

    #[error("Unexpected server message")]
    UnexpectedMessage,
}

#[derive(PartialEq, Eq)]
enum ClientState {
    Pending,
    Connected,
    Disconnected,
}

struct Client<C> {
    state: ClientState,
    last_input_step: u64,
    channel: C,
}

pub struct ServerSession<C, L> {
    listner: L,
    step_delta_ns: u64,
    current_step: u64,
    current_step_ns: u64,
    clients: HashMap<NonZeroU64, Client<C>>,
    next_client_id: NonZeroU64,
}

pub enum Event<'a, C, P: Schema, I: Schema> {
    ClientConnect(ClientConnectEvent<'a, C>),
    AddPlayer(AddPlayerEvent<'a, C, P>),
    Inputs(InputsEvent<'a, I>),
    Disconnected,
}

pub struct ClientConnectEvent<'a, C> {
    client: &'a mut Client<C>,
    step_delta_ns: u64,
    current_step: u64,
}

impl<C> ClientConnectEvent<'_, C>
where
    C: Channel,
{
    pub async fn accept(self, scope: &Scope<'_>) -> Result<(), C::Error> {
        self.client.state = ClientState::Connected;

        self.client
            .channel
            .send_reliable::<ServerMessage, _>(
                ServerMessageConnectedPack {
                    step: self.current_step,
                    step_delta_ns: self.step_delta_ns,
                },
                scope,
            )
            .await
    }
}

pub struct AddPlayerEvent<'a, C, P: Schema> {
    client: &'a mut Client<C>,
    player: Unpacked<'a, P>,
}

impl<'a, C, P> AddPlayerEvent<'a, C, P>
where
    C: Channel,
    P: Schema,
{
    pub fn player(&self) -> &Unpacked<'a, P> {
        &self.player
    }

    pub async fn accept<J, K>(self, info: K, scope: &Scope<'_>) -> Result<(), C::Error>
    where
        J: Schema,
        K: Pack<J>,
    {
        self.accept_with::<J, K, _>(|_| info, scope).await
    }

    pub async fn accept_with<J, K, F>(self, f: F, scope: &Scope<'_>) -> Result<(), C::Error>
    where
        J: Schema,
        K: Pack<J>,
        F: FnOnce(Unpacked<'a, P>) -> K,
    {
        self.try_accept_with(|player| Ok(f(player)), scope).await
    }

    pub async fn try_accept_with<J, K, F, E>(self, f: F, scope: &Scope<'_>) -> Result<(), E>
    where
        J: Schema,
        K: Pack<J>,
        F: FnOnce(Unpacked<'a, P>) -> Result<K, E>,
        E: From<C::Error>,
    {
        let info = f(self.player)?;

        self.client
            .channel
            .send_reliable::<ServerMessage<J>, _>(ServerMessagePlayerJoinedPack { info }, scope)
            .await?;

        Ok(())
    }
}

pub struct InputsEvent<'a, I: Schema> {
    inputs: SeqUnpacked<'a, (PlayerId, I)>,
}

impl<'a, I> InputsEvent<'a, I>
where
    I: Schema,
{
    pub fn inputs(&self) -> impl Iterator<Item = (PlayerId, Unpacked<'a, I>)> {
        self.inputs
            .clone()
            .filter_map(|(pid, input)| Some((pid?, input)))
    }
}

impl<C, L> ServerSession<C, L>
where
    C: Channel,
    L: Listner<Channel = C>,
{
    /// Create new server session via specified channel.
    pub fn new(listner: L, step_delta_ns: u64) -> Self {
        ServerSession {
            listner,
            step_delta_ns,
            current_step: 0,
            current_step_ns: 0,
            clients: HashMap::new(),
            next_client_id: unsafe {
                // # Safety
                // 1 is not zero
                NonZeroU64::new_unchecked(1)
            },
        }
    }

    /// Advances server-side simulation by one step.
    /// Broadcasts updates to all clients.
    pub async fn advance<U, K>(&mut self, delta_ns: u64, updates: K, scope: &Scope<'_>)
    where
        U: Schema,
        K: Pack<U> + Clone,
    {
        self.current_step_ns += delta_ns;
        if self.current_step_ns > self.step_delta_ns {
            let steps = self.current_step_ns / self.step_delta_ns;
            self.current_step_ns %= self.step_delta_ns;
            self.current_step += steps;

            for client in self.clients.values_mut() {
                if let ClientState::Connected = client.state {
                    let result = client
                        .channel
                        .send::<ServerMessage<(), U>, _>(
                            ServerMessageUpdatesPack {
                                updates: updates.clone(),
                                server_step: self.current_step,
                            },
                            scope,
                        )
                        .await;

                    if let Err(err) = result {
                        tracing::error!("Client channel error: {}", err);
                        client.state = ClientState::Disconnected;
                    }
                }
            }
        }
    }

    pub fn events<'a, P, I>(
        &'a mut self,
        scope: &'a Scope<'_>,
    ) -> Result<impl Iterator<Item = (ClientId, Event<'a, C, P, I>)> + 'a, L::Error>
    where
        P: Schema,
        I: Schema,
    {
        let step_delta_ns = self.step_delta_ns;
        let current_step = self.current_step;

        self.clients
            .retain(|_, client| client.state != ClientState::Disconnected);

        loop {
            match self.listner.try_accept()? {
                None => break,
                Some(channel) => {
                    let client = Client {
                        state: ClientState::Pending,
                        channel,
                        last_input_step: 0,
                    };

                    self.clients.insert(self.next_client_id, client);
                    self.next_client_id = NonZeroU64::new(self.next_client_id.get() + 1)
                        .expect("u64 overflow is unexpected");
                }
            }
        }

        let events = self.clients.iter_mut().filter_map(move |(&id, client)| {
            debug_assert!(!matches!(client.state, ClientState::Disconnected));

            let cid = ClientId(id);
            let msgs = client.channel.recv::<ClientMessage<P, I>>(scope);
            match msgs {
                Ok(Some(ClientMessageUnpacked::Connect { token: _ })) => {
                    if let ClientState::Pending = client.state {
                        Some((
                            cid,
                            Event::ClientConnect(ClientConnectEvent {
                                client,
                                step_delta_ns,
                                current_step,
                            }),
                        ))
                    } else {
                        client.state = ClientState::Disconnected;
                        Some((cid, Event::Disconnected))
                    }
                }
                Ok(Some(ClientMessageUnpacked::AddPlayer { player })) => {
                    if let ClientState::Connected = client.state {
                        Some((cid, Event::AddPlayer(AddPlayerEvent { client, player })))
                    } else {
                        Some((cid, Event::Disconnected))
                    }
                }
                Ok(Some(ClientMessageUnpacked::Inputs { step, inputs })) => {
                    if let ClientState::Connected = client.state {
                        if client.last_input_step < step {
                            client.last_input_step = step;
                            Some((cid, Event::Inputs(InputsEvent { inputs })))
                        } else {
                            None
                        }
                    } else {
                        Some((cid, Event::Disconnected))
                    }
                }
                Ok(None) => None,
                Err(err) => {
                    tracing::error!("Client error: {}", err);
                    client.state = ClientState::Disconnected;
                    None
                }
            }
        });

        Ok(events)
    }
}
