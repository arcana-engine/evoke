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
    next_update_step: u64,
}

impl<C> ClientSession<C>
where
    C: Channel,
{
    /// Create new client session via specified channel.
    pub async fn new(channel: C, scope: &Scope<'_>) -> Result<Self, ClientError<C::Error>> {
        let mut channel = channel;
        channel
            .send_reliable::<ClientMessage, _>(ClientMessageConnectPack { token: "lloth" }, scope)
            .await?;

        loop {
            match channel.recv::<ServerMessage>(&scope) {
                Ok(Some(ServerMessageUnpacked::Connected { step })) => {
                    return Ok(ClientSession {
                        channel,
                        current_step: step,
                        next_update_step: 0,
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
                    next_update_step: self.next_update_step,
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
        scope: &'a Scope<'_>,
    ) -> Result<Option<Updates<'a, U>>, ClientError<C::Error>>
    where
        U: Schema,
    {
        let updates = match self.channel.recv::<ServerMessage<(), U>>(scope) {
            Ok(Some(ServerMessageUnpacked::Updates {
                server_step,
                updates,
            })) => {
                self.next_update_step = server_step + 1;
                Some(Updates {
                    server_step,
                    updates,
                })
            }
            Ok(Some(_)) => return Err(ClientError::UnexpectedMessage),
            Ok(None) => None,
            Err(err) => return Err(err.into()),
        };

        self.current_step += 1;
        Ok(updates)
    }
}
