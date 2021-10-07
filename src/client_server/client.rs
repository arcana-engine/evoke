use std::error::Error;

use alkahest::{Schema, Unpacked};
use scoped_arena::Scope;

use crate::channel::Channel;

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum ClientError<E: Error + 'static> {
    #[error("Client channel error: {source}")]
    ChannelError {
        #[from]
        source: E,
    },

    #[error("Unexpected server message")]
    UnexpectedMessage,
}
pub struct Updates<'a, U: Schema> {
    pub server_step: u64,
    pub updates: Unpacked<'a, U>,
}

pub struct ClientSession<C> {
    channel: C,
    current_step: u64,
    step_delta_ns: u64,
    current_step_ns: u64,
}

impl<C> ClientSession<C>
where
    C: Channel,
{
    /// Create new client session via specified channel.
    pub async fn new(channel: C, scope: &Scope<'_>) -> Result<Self, ClientError<C::Error>> {
        let mut channel = channel;
        channel
            .send_reliable::<ClientMessage, _>(ClientMessageConnectPack { token: "astral" }, scope)
            .await?;

        loop {
            match channel.recv::<ServerMessage>(&scope) {
                Ok(Some(ServerMessageUnpacked::Connected {
                    step,
                    step_delta_ns,
                })) => {
                    return Ok(ClientSession {
                        channel,
                        current_step: step,
                        step_delta_ns,
                        current_step_ns: 0,
                    });
                }
                Ok(Some(ServerMessageUnpacked::Updates { .. })) => {}
                Ok(Some(ServerMessageUnpacked::PlayerJoined { .. })) => {
                    return Err(ClientError::UnexpectedMessage)
                }
                Ok(None) => {
                    channel.recv_ready().await?;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    pub fn current_step(&self) -> u64 {
        self.current_step
    }

    /// Adds new player to the session.
    pub async fn add_player<'a, P, J, K>(
        &mut self,
        player: K,
        scope: &'a Scope<'_>,
    ) -> Result<Unpacked<'a, J>, ClientError<C::Error>>
    where
        P: Schema,
        J: Schema,
        K: Pack<P>,
    {
        self.channel
            .send_reliable::<ClientMessage<P>, _>(ClientMessageAddPlayerPack { player }, scope)
            .await?;

        loop {
            match self.channel.recv::<ServerMessage<J>>(scope) {
                Ok(Some(ServerMessageUnpacked::PlayerJoined { info })) => return Ok(info),
                Ok(Some(ServerMessageUnpacked::Connected { .. })) => {
                    return Err(ClientError::UnexpectedMessage)
                }
                Ok(Some(ServerMessageUnpacked::Updates { .. })) => {}
                Ok(None) => {
                    self.channel.recv_ready().await?;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Sends input from players to the server.
    pub async fn send_inputs<T, K, I>(
        &mut self,
        inputs: I,
        scope: &Scope<'_>,
    ) -> Result<(), C::Error>
    where
        T: Schema,
        K: Pack<T>,
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator<Item = (PlayerId, K)>,
    {
        self.channel
            .send_reliable::<ClientMessage<(), T>, _>(
                ClientMessageInputsPack {
                    step: self.current_step,
                    inputs: inputs.into_iter(),
                },
                scope,
            )
            .await?;

        Ok(())
    }

    /// Advances client-side simulation by one step.
    pub fn advance<'a, U>(
        &mut self,
        delta_ns: u64,
        scope: &'a Scope<'_>,
    ) -> Result<Option<Updates<'a, U>>, ClientError<C::Error>>
    where
        U: Schema,
    {
        self.current_step_ns += delta_ns;

        let steps = self.current_step_ns / self.step_delta_ns;
        self.current_step_ns %= self.step_delta_ns;
        self.current_step += steps;

        match self.channel.recv::<ServerMessage<(), U>>(scope) {
            Ok(Some(ServerMessageUnpacked::Updates {
                server_step,
                updates,
            })) => Ok(Some(Updates {
                server_step,
                updates,
            })),
            Ok(Some(_)) => Err(ClientError::UnexpectedMessage),
            Ok(None) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}
