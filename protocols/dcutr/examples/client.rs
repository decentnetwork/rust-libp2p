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

use futures::executor::block_on;
use futures::future::FutureExt;
use futures::stream::StreamExt;
use libp2p::core::multiaddr::{Multiaddr, Protocol};
use libp2p::core::upgrade;
use libp2p::dcutr;
use libp2p::dns::DnsConfig;
use libp2p::identify::{Identify, IdentifyConfig, IdentifyEvent, IdentifyInfo};
use libp2p::noise;
use libp2p::ping::{Ping, PingConfig, PingEvent};
use libp2p::relay::v2::client::{self, Client};
use libp2p::swarm::{SwarmBuilder, SwarmEvent};
use libp2p::tcp::TcpConfig;
use libp2p::Transport;
use libp2p::{identity, NetworkBehaviour, PeerId};
use log::info;
use std::convert::TryInto;
use std::error::Error;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::task::{Context, Poll};
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "libp2p DCUtR client")]
struct Opts {
    /// The mode (relay, client-listen, client-dial)
    #[structopt(long)]
    mode: Mode,

    /// Fixed value to generate deterministic peer id
    #[structopt(long)]
    secret_key_seed: u8,

    /// The listening address
    #[structopt(long)]
    relay_address: Multiaddr,

    /// Peer ID of the remote peer to hole punch to.
    #[structopt(long)]
    remote_peer_id: Option<PeerId>,
}

#[derive(Debug, StructOpt)]
enum Mode {
    Dial,
    Listen,
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(mode: &str) -> Result<Self, Self::Err> {
        match mode {
            "dial" => Ok(Mode::Dial),
            "listen" => Ok(Mode::Listen),
            _ => Err("Expected either 'dial' or 'listen'".to_string()),
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let opts = Opts::from_args();

    let local_key = generate_ed25519(opts.secret_key_seed);
    let local_peer_id = PeerId::from(local_key.public());
    info!("Local peer id: {:?}", local_peer_id);

    let (transport, client) = Client::new_transport_and_behaviour(
        local_peer_id,
        block_on(DnsConfig::system(TcpConfig::new().port_reuse(true))).unwrap(),
    );

    let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
        .into_authentic(&local_key)
        .expect("Signing libp2p-noise static DH keypair failed.");

    let transport = transport
        .upgrade()
        .authenticate_with_version(
            noise::NoiseConfig::xx(noise_keys).into_authenticated(),
            upgrade::AuthenticationVersion::V1SimultaneousOpen,
        )
        .multiplex(libp2p_yamux::YamuxConfig::default())
        .boxed();

    #[derive(NetworkBehaviour)]
    #[behaviour(out_event = "Event", event_process = false)]
    struct Behaviour {
        relay_client: Client,
        ping: Ping,
        identify: Identify,
        dcutr: dcutr::behaviour::Behaviour,
    }

    #[derive(Debug)]
    enum Event {
        Ping(PingEvent),
        Identify(IdentifyEvent),
        Relay(client::Event),
        Dcutr(dcutr::behaviour::Event),
    }

    impl From<PingEvent> for Event {
        fn from(e: PingEvent) -> Self {
            Event::Ping(e)
        }
    }

    impl From<IdentifyEvent> for Event {
        fn from(e: IdentifyEvent) -> Self {
            Event::Identify(e)
        }
    }

    impl From<client::Event> for Event {
        fn from(e: client::Event) -> Self {
            Event::Relay(e)
        }
    }

    impl From<dcutr::behaviour::Event> for Event {
        fn from(e: dcutr::behaviour::Event) -> Self {
            Event::Dcutr(e)
        }
    }

    let behaviour = Behaviour {
        relay_client: client,
        ping: Ping::new(PingConfig::new()),
        identify: Identify::new(IdentifyConfig::new(
            "/TODO/0.0.1".to_string(),
            local_key.public(),
        )),
        dcutr: dcutr::behaviour::Behaviour::new(),
    };

    let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
        .dial_concurrency_factor(10_u8.try_into().unwrap())
        .build();

    swarm
        .listen_on(
            Multiaddr::empty()
                .with("0.0.0.0".parse::<Ipv4Addr>().unwrap().into())
                .with(Protocol::Tcp(0)),
        )
        .unwrap();

    // Wait to listen on localhost.
    block_on(async {
        let mut delay = futures_timer::Delay::new(std::time::Duration::from_secs(1)).fuse();
        loop {
            futures::select! {
                            event = swarm.next() => {
                                match event.unwrap() {
                                    SwarmEvent::NewListenAddr { address, .. } => {
            info!("Listening on {:?}", address);
                                    }
                                    event => panic!("{:?}", event),
                                }
                            }
                            _ = delay => {
                                break;
                            }
                        }
        }
    });

    match opts.mode {
        Mode::Dial => {
            swarm.dial(opts.relay_address.clone()).unwrap();
        }
        Mode::Listen => {
            swarm
                .listen_on(opts.relay_address.clone().with(Protocol::P2pCircuit))
                .unwrap();
        }
    }

    // Wait till connected to relay to learn external address.
    block_on(async {
        loop {
            match swarm.next().await.unwrap() {
                SwarmEvent::NewListenAddr { .. } => {}
                SwarmEvent::Dialing { .. } => {}
                SwarmEvent::ConnectionEstablished { .. } => {}
                SwarmEvent::Behaviour(Event::Ping(_)) => {}
                SwarmEvent::Behaviour(Event::Relay(_)) => {}
                SwarmEvent::Behaviour(Event::Identify(IdentifyEvent::Sent { .. })) => {}
                SwarmEvent::Behaviour(Event::Identify(IdentifyEvent::Received {
                    info: IdentifyInfo { observed_addr, .. },
                    ..
                })) => {
                    info!("Observed address: {:?}", observed_addr);
                    break;
                }
                event => panic!("{:?}", event),
            }
        }
    });

    if matches!(opts.mode, Mode::Dial) {
        swarm
            .dial(
                opts.relay_address
                    .clone()
                    .with(Protocol::P2pCircuit)
                    .with(Protocol::P2p(opts.remote_peer_id.unwrap().into())),
            )
            .unwrap();
    }

    block_on(futures::future::poll_fn(move |cx: &mut Context<'_>| {
        loop {
            match swarm.poll_next_unpin(cx) {
                Poll::Ready(Some(SwarmEvent::NewListenAddr { address, .. })) => {
                    info!("Listening on {:?}", address);
                }
                Poll::Ready(Some(SwarmEvent::Behaviour(Event::Relay(event)))) => {
                    info!("{:?}", event)
                }
                Poll::Ready(Some(SwarmEvent::Behaviour(Event::Dcutr(event)))) => {
                    info!("{:?}", event)
                }
                Poll::Ready(Some(SwarmEvent::Behaviour(Event::Identify(event)))) => {
                    info!("{:?}", event)
                }
                Poll::Ready(Some(SwarmEvent::Behaviour(Event::Ping(_)))) => {}
                Poll::Ready(Some(SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                })) => {
                    info!("Established connection to {:?} via {:?}", peer_id, endpoint);
                }
                Poll::Ready(Some(SwarmEvent::OutgoingConnectionError { peer_id, error })) => {
                    info!("Outgoing connection error to {:?}: {:?}", peer_id, error);
                }
                Poll::Ready(Some(e)) => {
                    // panic!("{:?}", e)
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => {
                    break;
                }
            }
        }
        Poll::Pending
    }))
}

fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;

    let secret_key = identity::ed25519::SecretKey::from_bytes(&mut bytes)
        .expect("this returns `Err` only if the length is wrong; the length is correct; qed");
    identity::Keypair::Ed25519(secret_key.into())
}
