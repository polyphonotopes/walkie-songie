//! Relay server with gossipsub for browser-to-browser WebRTC signaling.

use std::time::Duration;
use std::net::{Ipv4Addr, Ipv6Addr};

use clap::Parser;
use futures::StreamExt;
use libp2p::{
    core::multiaddr::Protocol,
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identify, identity, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId,
};
use tracing_subscriber::EnvFilter;

#[derive(NetworkBehaviour)]
struct Behaviour {
    relay: relay::Behaviour,
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    gossipsub: gossipsub::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let opt = Opt::parse();

    let local_key = generate_ed25519(opt.secret_key_seed);
    let local_peer_id = PeerId::from(local_key.public());
    tracing::info!("Relay peer ID: {}", local_peer_id);

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(local_key.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_dns()?
        .with_websocket(noise::Config::new, yamux::Config::default)
        .await?
        .with_behaviour(|key| {
            // Gossipsub config
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(1))
                .validation_mode(gossipsub::ValidationMode::Permissive)
                .build()
                .map_err(|e| std::io::Error::other(format!("Gossipsub config: {e}")))?;

            let gossipsub = gossipsub::Behaviour::new(
                MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .map_err(|e| std::io::Error::other(format!("Gossipsub: {e}")))?;

            Ok(Behaviour {
                relay: relay::Behaviour::new(key.public().to_peer_id(), Default::default()),
                ping: ping::Behaviour::new(ping::Config::new()),
                identify: identify::Behaviour::new(identify::Config::new(
                    "/walkie-songie-relay/1.0.0".to_string(),
                    key.public(),
                )),
                gossipsub,
            })
        })?
        .build();

    // Listen on TCP
    let tcp_addr = Multiaddr::empty()
        .with(match opt.use_ipv6 {
            true => Protocol::from(Ipv6Addr::UNSPECIFIED),
            false => Protocol::from(Ipv4Addr::UNSPECIFIED),
        })
        .with(Protocol::Tcp(opt.port));
    swarm.listen_on(tcp_addr)?;

    // Listen on WebSocket
    if let Some(ws_port) = opt.ws_port {
        let ws_addr = Multiaddr::from(Ipv4Addr::UNSPECIFIED)
            .with(Protocol::Tcp(ws_port))
            .with(Protocol::Ws(std::borrow::Cow::Borrowed("/")));
        swarm.listen_on(ws_addr)?;
    }

    tracing::info!("Relay started with gossipsub support");

    loop {
        match swarm.next().await.expect("Infinite stream") {
            SwarmEvent::Behaviour(event) => {
                match &event {
                    BehaviourEvent::Identify(identify::Event::Received {
                        info: identify::Info { observed_addr, .. },
                        ..
                    }) => {
                        swarm.add_external_address(observed_addr.clone());
                    }
                    BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, topic }) => {
                        tracing::info!("Peer {} subscribed to {}", peer_id, topic);
                        // Auto-subscribe to topics that peers subscribe to
                        let topic = IdentTopic::new(topic.to_string());
                        let _ = swarm.behaviour_mut().gossipsub.subscribe(&topic);
                    }
                    BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        propagation_source,
                        message,
                        ..
                    }) => {
                        tracing::debug!(
                            "Relaying gossipsub message from {} on {} ({} bytes)",
                            propagation_source,
                            message.topic,
                            message.data.len()
                        );
                    }
                    _ => {}
                }
                tracing::debug!("{:?}", event);
            }
            SwarmEvent::NewListenAddr { mut address, .. } => {
                address.push(Protocol::P2p(local_peer_id));
                tracing::info!("Listening on {:?}", address);
            }
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                tracing::info!("Connected: {} via {}", peer_id, endpoint.get_remote_address());
            }
            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                tracing::info!("Disconnected: {} ({:?})", peer_id, cause);
            }
            _ => {}
        }
    }
}

fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;
    identity::Keypair::ed25519_from_bytes(bytes).expect("valid seed")
}

#[derive(Debug, Parser)]
#[command(name = "walkie-songie-relay")]
struct Opt {
    #[arg(long, default_value = "false")]
    use_ipv6: bool,

    #[arg(long, default_value = "42")]
    secret_key_seed: u8,

    #[arg(long, default_value = "9000")]
    port: u16,

    #[arg(long)]
    ws_port: Option<u16>,
}
