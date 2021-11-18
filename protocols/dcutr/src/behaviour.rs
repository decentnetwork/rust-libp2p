// Copyright 2021 Protocol Labs.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! [`NetworkBehaviour`] to act as a direct connection upgrade through relay node.

use crate::handler;
use either::Either;
use libp2p_core::connection::{ConnectedPoint, ConnectionId};
use libp2p_core::multiaddr::Protocol;
use libp2p_core::{Multiaddr, PeerId};
use libp2p_swarm::dial_opts::{self, DialOpts};
use libp2p_swarm::{
    DialError, IntoProtocolsHandler, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler,
    PollParameters, ProtocolsHandler,
};
use std::collections::VecDeque;
use std::task::{Context, Poll};

/// The events produced by the [`Behaviour`].
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    InitiateDirectConnectionUpgrade {
        remote_peer_id: PeerId,
        local_relayed_addr: Multiaddr,
    },
    RemoteInitiatedDirectConnectionUpgrade {
        remote_peer_id: PeerId,
        remote_relayed_addr: Multiaddr,
    },
    // TODO: Emit
    DirectConnectionUpgradeSucceeded,
    DirectConnectionUpgradeFailed {
        remote_peer_id: PeerId,
    },
}

pub struct Behaviour {
    /// Queue of actions to return when polled.
    queued_actions: VecDeque<
        NetworkBehaviourAction<
            <Self as NetworkBehaviour>::OutEvent,
            <Self as NetworkBehaviour>::ProtocolsHandler,
        >,
    >,
}

impl Behaviour {
    pub fn new() -> Self {
        Behaviour {
            queued_actions: Default::default(),
        }
    }
}

impl NetworkBehaviour for Behaviour {
    type ProtocolsHandler = handler::Prototype;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        handler::Prototype::UnknownConnection
    }

    fn addresses_of_peer(&mut self, _peer_id: &PeerId) -> Vec<Multiaddr> {
        vec![]
    }

    fn inject_connected(&mut self, _peer_id: &PeerId) {}

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        connection_id: &ConnectionId,
        connected_point: &ConnectedPoint,
        _failed_addresses: Option<&Vec<Multiaddr>>,
    ) {
        match connected_point {
            ConnectedPoint::Listener { local_addr, .. }
                if local_addr.iter().any(|p| p == Protocol::P2pCircuit) =>
            {
                // TODO: Try dialing the remote peer directly. Specification:
                //
                // > The protocol starts with the completion of a relay connection from A to B. Upon
                // observing the new connection, the inbound peer (here B) checks the addresses
                // advertised by A via identify. If that set includes public addresses, then A may
                // be reachable by a direct connection, in which case B attempts a unilateral
                // connection upgrade by initiating a direct connection to A.
                //
                // https://github.com/libp2p/specs/blob/master/relay/DCUtR.md#the-protocol
                //
                // TODO: Only do this in case there is not already a direct connection.
                self.queued_actions
                    .push_back(NetworkBehaviourAction::NotifyHandler {
                        peer_id: *peer_id,
                        handler: NotifyHandler::One(*connection_id),
                        event: Either::Left(handler::In::Connect {
                            obs_addrs: vec![],
                            attempt: 1,
                        }),
                    });
                self.queued_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::InitiateDirectConnectionUpgrade {
                            remote_peer_id: *peer_id,
                            local_relayed_addr: local_addr.clone(),
                        },
                    ));
            }
            _ => {}
        }
    }

    fn inject_dial_failure(
        &mut self,
        peer_id: Option<PeerId>,
        handler: Self::ProtocolsHandler,
        _error: &DialError,
    ) {
        match handler {
            handler::Prototype::DirectConnection {
                role:
                    handler::Role::Initiator {
                        attempt,
                        relay_connection_id,
                    },
            } => {
                let peer_id =
                    peer_id.expect("Prototype::DirectConnection to always connect to known peer.");
                if attempt < 3 {
                    // TODO: Emit event that attempt failed and another attempt is started.
                    self.queued_actions
                        .push_back(NetworkBehaviourAction::NotifyHandler {
                            peer_id: peer_id,
                            handler: NotifyHandler::One(relay_connection_id),
                            event: Either::Left(handler::In::Connect {
                                obs_addrs: vec![],
                                attempt: attempt + 1,
                            }),
                        });
                } else {
                    self.queued_actions
                        .push_back(NetworkBehaviourAction::GenerateEvent(
                            Event::DirectConnectionUpgradeFailed {
                                remote_peer_id: peer_id,
                            },
                        ));
                }
            }
            _ => {}
        }
    }

    fn inject_disconnected(&mut self, _peer: &PeerId) {
        todo!();
    }

    fn inject_connection_closed(
        &mut self,
        _peer_id: &PeerId,
        _connection_id: &ConnectionId,
        _: &ConnectedPoint,
        _handler: <<Self as NetworkBehaviour>::ProtocolsHandler as IntoProtocolsHandler>::Handler,
    ) {
        todo!();
    }

    fn inject_event(
        &mut self,
        event_source: PeerId,
        connection: ConnectionId,
        handler_event: <<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::OutEvent,
    ) {
        let handler_event = match handler_event {
            Either::Left(event) => event,
            Either::Right(event) => void::unreachable(event),
        };

        match handler_event {
            handler::Event::InboundConnectReq {
                inbound_connect,
                remote_addr,
            } => {
                self.queued_actions
                    .push_back(NetworkBehaviourAction::NotifyHandler {
                        peer_id: event_source,
                        handler: NotifyHandler::One(connection),
                        event: Either::Left(handler::In::AcceptInboundConnect {
                            inbound_connect,
                            obs_addrs: vec![],
                        }),
                    });
                self.queued_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::RemoteInitiatedDirectConnectionUpgrade {
                            remote_peer_id: event_source,
                            remote_relayed_addr: remote_addr,
                        },
                    ));
            }
            handler::Event::InboundConnectNeg(remote_addrs) => {
                self.queued_actions.push_back(NetworkBehaviourAction::Dial {
                    // TODO: Handle empty addresses.
                    opts: DialOpts::peer_id(event_source)
                        .addresses(remote_addrs)
                        .condition(dial_opts::PeerCondition::Always)
                        .build(),
                    handler: handler::Prototype::DirectConnection {
                        role: handler::Role::Listener,
                    },
                });
            }
            handler::Event::OutboundConnectNeg {
                remote_addrs,
                attempt,
            } => {
                self.queued_actions.push_back(NetworkBehaviourAction::Dial {
                    // TODO: Handle empty addresses.
                    opts: DialOpts::peer_id(event_source)
                        .condition(dial_opts::PeerCondition::Always)
                        .addresses(remote_addrs)
                        .build(),
                    handler: handler::Prototype::DirectConnection {
                        role: handler::Role::Initiator {
                            attempt: attempt,
                            relay_connection_id: connection,
                        },
                    },
                });
            }
        }
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
        poll_parameters: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ProtocolsHandler>> {
        if let Some(mut event) = self.queued_actions.pop_front() {
            // Set obs addresses.
            if let NetworkBehaviourAction::NotifyHandler {
                event:
                    Either::Left(handler::In::Connect {
                        ref mut obs_addrs, ..
                    }),
                ..
            }
            | NetworkBehaviourAction::NotifyHandler {
                event:
                    Either::Left(handler::In::AcceptInboundConnect {
                        ref mut obs_addrs, ..
                    }),
                ..
            } = &mut event
            {
                *obs_addrs = poll_parameters
                    .external_addresses()
                    .map(|a| {
                        a.addr
                            .with(Protocol::P2p((*poll_parameters.local_peer_id()).into()))
                    })
                    .collect();
            }

            return Poll::Ready(event);
        }

        Poll::Pending
    }
}