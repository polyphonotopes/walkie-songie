//! P2P synchronization using libp2p with browser-to-browser WebRTC.
//!
//! Uses circuit relay for signaling, then upgrades to direct WebRTC.
//! Gossipsub broadcasts yrs CRDT updates to peers in a topic.

use std::sync::Arc;
use std::time::Duration;

use futures::{channel::mpsc, task::AtomicWaker, FutureExt, StreamExt};
use futures_signals::signal::Mutable;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identify,
    multiaddr::Protocol,
    swarm::SwarmEvent,
    Multiaddr, Swarm, Transport,
};
use libp2p_core::{muxing::StreamMuxerBox, upgrade::Version};
use libp2p_swarm::NetworkBehaviour;
use libp2p_webrtc_websys::browser::{self, Behaviour as WebRTCBehaviour, SignalingConfig, Transport as BrowserWebrtcTransport};
use wasm_bindgen_futures::spawn_local;

use crate::room::{RoomState, YrsRoomState};

/// Relay server address (WebSocket with TLS)
/// Note: No peer ID suffix - we accept whatever peer the server presents.
/// For production with peer ID verification, append /p2p/<peer-id>
const RELAY_ADDR: &str = "/dns4/libp2p.wondering.xyz/tcp/443/wss";

/// Combined behaviour for browser P2P.
#[derive(NetworkBehaviour)]
struct Behaviour {
    relay: libp2p_relay::client::Behaviour,
    webrtc: WebRTCBehaviour,
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
}

/// Hash a topic name to a gossipsub topic.
fn topic_from_room(room_name: &str) -> IdentTopic {
    IdentTopic::new(format!("walkie-songie/{}", room_name))
}

/// Start libp2p room sync in the background.
pub fn start_libp2p_room_sync(
    room: Mutable<YrsRoomState>,
    room_topic: String,
    peer_id_out: Mutable<Option<String>>,
    room_version: Mutable<u64>,
) {
    spawn_local(async move {
        match run_sync(room, &room_topic, peer_id_out, room_version).await {
            Ok(()) => {
                web_sys::console::log_1(&"[libp2p] Sync completed".into());
            }
            Err(e) => {
                web_sys::console::error_1(&format!("[libp2p] Sync failed: {e}").into());
            }
        }
    });
}

async fn run_sync(
    room: Mutable<YrsRoomState>,
    room_topic: &str,
    peer_id_out: Mutable<Option<String>>,
    room_version: Mutable<u64>,
) -> Result<(), String> {
    let room_name = room_topic.split('@').next().unwrap_or(room_topic);

    web_sys::console::log_1(&format!("[libp2p] Starting browser-to-browser sync for room: {room_name}").into());

    // Generate keypair
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let local_peer_id = keypair.public().to_peer_id();

    web_sys::console::log_1(&format!("[libp2p] Local peer ID: {local_peer_id}").into());
    peer_id_out.set(Some(local_peer_id.to_string()));

    let transport_waker = Arc::new(AtomicWaker::new());

    // Create relay client transport and behaviour
    let (relay_transport, relay_behaviour) = libp2p_relay::client::new(local_peer_id);

    let relay_transport_upgraded = relay_transport
        .upgrade(Version::V1)
        .authenticate(libp2p::noise::Config::new(&keypair).map_err(|e| format!("Noise config: {e}"))?)
        .multiplex(libp2p::yamux::Config::default())
        .boxed();

    // Create WebSocket transport for relay connection
    let ws_transport = libp2p_websocket_websys::Transport::default()
        .upgrade(Version::V1)
        .authenticate(libp2p::noise::Config::new(&keypair).map_err(|e| format!("Noise config: {e}"))?)
        .multiplex(libp2p::yamux::Config::default())
        .boxed();

    // Create WebRTC transport for direct browser-to-browser
    let webrtc_config = libp2p_webrtc_websys::browser::Config {
        keypair: keypair.clone(),
    };

    let stun_servers = ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"];

    let signaling_config = SignalingConfig::new(
        3,  // max retries
        100, // max ice gathering attempts
        Duration::from_millis(0), // signaling delay
        Duration::from_millis(100), // connection check delay
        300, // max connection checks (30 seconds)
        Duration::from_secs(10), // ICE gathering timeout
        local_peer_id,
        Some(stun_servers.iter().map(ToString::to_string).collect()),
    );

    let (webrtc_transport, webrtc_behaviour) =
        BrowserWebrtcTransport::new(webrtc_config, signaling_config, transport_waker);
    let webrtc_transport_boxed = webrtc_transport.boxed();

    // Gossipsub for pub/sub
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1))
        .validation_mode(gossipsub::ValidationMode::Permissive)
        .build()
        .map_err(|e| format!("Gossipsub config: {e}"))?;

    let gossipsub = gossipsub::Behaviour::new(
        MessageAuthenticity::Signed(keypair.clone()),
        gossipsub_config,
    )
    .map_err(|e| format!("Gossipsub: {e}"))?;

    // Identify protocol
    let identify = identify::Behaviour::new(identify::Config::new(
        "/walkie-songie/1.0.0".to_string(),
        keypair.public(),
    ));

    let behaviour = Behaviour {
        relay: relay_behaviour,
        webrtc: webrtc_behaviour,
        gossipsub,
        identify,
    };

    // Combined transport: WebRTC || Relay || WebSocket
    let final_transport = webrtc_transport_boxed
        .or_transport(relay_transport_upgraded)
        .or_transport(ws_transport)
        .map(|either_output, _| match either_output {
            futures::future::Either::Left(futures::future::Either::Left((peer_id, conn))) => {
                (peer_id, StreamMuxerBox::new(conn))
            }
            futures::future::Either::Left(futures::future::Either::Right(output)) => output,
            futures::future::Either::Right(output) => output,
        })
        .boxed();

    let mut swarm = Swarm::new(
        final_transport,
        behaviour,
        local_peer_id,
        libp2p::swarm::Config::with_executor(Box::new(|fut| {
            spawn_local(fut);
        }))
        .with_idle_connection_timeout(Duration::from_secs(300)),
    );

    // Subscribe to the room topic
    let topic = topic_from_room(room_name);
    swarm.behaviour_mut().gossipsub.subscribe(&topic)
        .map_err(|e| format!("Subscribe error: {e:?}"))?;

    web_sys::console::log_1(&format!("[libp2p] Subscribed to topic: {}", topic.hash()).into());

    // Parse relay address
    let relay_addr: Multiaddr = RELAY_ADDR.parse()
        .map_err(|e| format!("Invalid relay addr: {e}"))?;

    // First, dial the relay server to establish a connection
    web_sys::console::log_1(&format!("[libp2p] Dialing relay: {relay_addr}").into());

    if let Err(e) = swarm.dial(relay_addr.clone()) {
        web_sys::console::warn_1(&format!("[libp2p] Failed to dial relay: {e}").into());
    }

    // Listen for WebRTC connections
    let webrtc_addr: Multiaddr = "/webrtc".parse().unwrap();
    if let Err(e) = swarm.listen_on(webrtc_addr) {
        web_sys::console::warn_1(&format!("[libp2p] Failed to listen on WebRTC: {e}").into());
    }

    // Get notification receiver for local changes
    let mut notify_rx = room.lock_ref().subscribe();
    let mut last_broadcast_sv = room.lock_ref().state_vector();

    // Event loop
    loop {
        let timeout = gloo_timers::future::TimeoutFuture::new(50);

        futures::select! {
            event = swarm.select_next_some() => {
                handle_swarm_event(&mut swarm, event, &room, &room_version, &topic, &relay_addr, local_peer_id, &mut last_broadcast_sv);
            }

            _ = timeout.fuse() => {
                // Check for local changes
                if notify_rx.has_changed().unwrap_or(false) {
                    let _ = notify_rx.borrow_and_update();

                    if let Ok(update) = room.lock_ref().encode_diff(&last_broadcast_sv) {
                        if !update.is_empty() {
                            web_sys::console::log_1(&format!(
                                "[libp2p] Broadcasting update ({} bytes)",
                                update.len()
                            ).into());

                            if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), update) {
                                web_sys::console::warn_1(&format!(
                                    "[libp2p] Failed to publish: {e:?}"
                                ).into());
                            }

                            last_broadcast_sv = room.lock_ref().state_vector();
                        }
                    }
                }
            }
        }
    }
}

fn handle_swarm_event(
    swarm: &mut Swarm<Behaviour>,
    event: SwarmEvent<BehaviourEvent>,
    room: &Mutable<YrsRoomState>,
    room_version: &Mutable<u64>,
    topic: &IdentTopic,
    relay_addr: &Multiaddr,
    local_peer_id: libp2p::PeerId,
    last_broadcast_sv: &mut Vec<u8>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            web_sys::console::log_1(&format!("[libp2p] Listening on: {address}").into());

            // If this is a circuit address, generate the WebRTC dial address
            if address.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                let webrtc_addr = format!(
                    "{}/p2p-circuit/webrtc/p2p/{}",
                    relay_addr,
                    local_peer_id
                );
                web_sys::console::log_1(&format!("[libp2p] WebRTC address: {webrtc_addr}").into());
            }
        }

        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
            let addr = endpoint.get_remote_address().to_string();

            if addr.contains("/webrtc") && !addr.contains("/p2p-circuit") {
                web_sys::console::log_1(&format!(
                    "[libp2p] Direct WebRTC connection with: {peer_id}"
                ).into());
            } else if addr.contains("/p2p-circuit") {
                web_sys::console::log_1(&format!(
                    "[libp2p] Relay circuit connection with: {peer_id}"
                ).into());
            } else if addr.contains("/wss") || addr.contains("/ws") {
                web_sys::console::log_1(&format!(
                    "[libp2p] Connected to relay: {peer_id}"
                ).into());

                // Now that we're connected to the relay, listen on the circuit
                // IMPORTANT: Must include relay peer ID in the circuit address
                let circuit_addr = relay_addr.clone()
                    .with(Protocol::P2p(peer_id))
                    .with(Protocol::P2pCircuit);
                web_sys::console::log_1(&format!(
                    "[libp2p] Requesting circuit reservation: {circuit_addr}"
                ).into());

                if let Err(e) = swarm.listen_on(circuit_addr) {
                    web_sys::console::warn_1(&format!(
                        "[libp2p] Failed to listen on circuit: {e}"
                    ).into());
                }
            } else {
                web_sys::console::log_1(&format!(
                    "[libp2p] Connected to: {peer_id} via {addr}"
                ).into());
            }

            // Send current state to new peer
            let state_update = room.lock_ref().encode_state_as_update();
            if !state_update.is_empty() {
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), state_update) {
                    web_sys::console::warn_1(&format!(
                        "[libp2p] Failed to send state: {e:?}"
                    ).into());
                }
            }
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            web_sys::console::log_1(&format!(
                "[libp2p] Disconnected from {peer_id}: {cause:?}"
            ).into());
        }

        SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
            propagation_source,
            message,
            ..
        })) => {
            web_sys::console::log_1(&format!(
                "[libp2p] Received from {propagation_source} ({} bytes)",
                message.data.len()
            ).into());

            if let Err(e) = room.lock_mut().apply_update(&message.data) {
                web_sys::console::warn_1(&format!(
                    "[libp2p] Failed to apply update: {e}"
                ).into());
            } else {
                *last_broadcast_sv = room.lock_ref().state_vector();
                room_version.set(room_version.get() + 1);
            }
        }

        SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed {
            peer_id,
            topic: t,
        })) => {
            web_sys::console::log_1(&format!(
                "[libp2p] Peer {peer_id} subscribed to {t}"
            ).into());

            // Send state to newly subscribed peer
            let state_update = room.lock_ref().encode_state_as_update();
            if !state_update.is_empty() {
                web_sys::console::log_1(&format!(
                    "[libp2p] Sending state to new subscriber ({} bytes)",
                    state_update.len()
                ).into());
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), state_update) {
                    web_sys::console::warn_1(&format!(
                        "[libp2p] Failed to send to subscriber: {e:?}"
                    ).into());
                }
            }
        }

        SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::GossipsubNotSupported {
            peer_id,
        })) => {
            web_sys::console::warn_1(&format!(
                "[libp2p] Peer {peer_id} does NOT support gossipsub"
            ).into());
        }

        SwarmEvent::Behaviour(BehaviourEvent::Webrtc(webrtc_event)) => {
            match webrtc_event {
                browser::SignalingEvent::NewWebRTCConnection { peer_id } => {
                    web_sys::console::log_1(&format!(
                        "[libp2p] WebRTC signaling complete with: {peer_id}"
                    ).into());
                }
                browser::SignalingEvent::WebRTCConnectionError { peer_id, error } => {
                    web_sys::console::warn_1(&format!(
                        "[libp2p] WebRTC error with {peer_id}: {error}"
                    ).into());
                }
            }
        }

        SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            let protocols: Vec<_> = info.protocols.iter().map(|p| p.to_string()).collect();
            web_sys::console::log_1(&format!(
                "[libp2p] Identified {peer_id}: {:?}",
                protocols
            ).into());

            // Log the peer's listen addresses (for discovery)
            for addr in &info.listen_addrs {
                web_sys::console::log_1(&format!(
                    "[libp2p] Peer {peer_id} listen addr: {addr}"
                ).into());
            }
        }

        _ => {}
    }
}
