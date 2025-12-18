//! Y-WebRTC compatible signaller for matchbox.
//!
//! Uses the simple y-webrtc signaling protocol to connect to public servers
//! like wss://signaling.yjs.dev

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use matchbox_socket::{PeerEvent, PeerId, PeerRequest, PeerSignal, SignalingError, Signaller, SignallerBuilder};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Y-webrtc signaling servers.
const SIGNALING_SERVERS: &[&str] = &[
    "wss://signal.wondering.xyz",
];

/// Messages sent to the signaling server.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SignalMessage {
    Subscribe { topics: Vec<String> },
    Publish { topic: String, data: SignalData },
    Pong,
}

/// Signal data we publish.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SignalData {
    from: String,
    to: Option<String>,
    signal: SignalPayload,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SignalPayload {
    Announce,
    Offer { sdp: String },
    Answer { sdp: String },
    Candidate { candidate: String },
}

/// Messages received from the signaling server.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMessage {
    Publish { topic: String, data: SignalData },
    Ping,
    Pong,
    #[serde(other)]
    Other,
}

/// Builder for y-webrtc signaller.
#[derive(Clone, Debug, Default)]
pub struct YjsSignallerBuilder;

impl YjsSignallerBuilder {
    pub fn new() -> Self {
        Self
    }
}

/// Wait for WebSocket to be open
async fn wait_for_open(ws: &web_sys::WebSocket) -> Result<(), String> {
    use futures::channel::oneshot;

    let (tx, rx) = oneshot::channel::<Result<(), String>>();
    let tx = Rc::new(RefCell::new(Some(tx)));

    // Check if already open
    if ws.ready_state() == web_sys::WebSocket::OPEN {
        return Ok(());
    }

    // Check if already closed/closing
    if ws.ready_state() >= web_sys::WebSocket::CLOSING {
        return Err("WebSocket already closed".into());
    }

    let tx_open = tx.clone();
    let onopen = Closure::wrap(Box::new(move || {
        if let Some(tx) = tx_open.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    }) as Box<dyn FnMut()>);

    let tx_error = tx.clone();
    let onerror = Closure::wrap(Box::new(move || {
        if let Some(tx) = tx_error.borrow_mut().take() {
            let _ = tx.send(Err("WebSocket connection failed".into()));
        }
    }) as Box<dyn FnMut()>);

    let tx_close = tx;
    let onclose = Closure::wrap(Box::new(move || {
        if let Some(tx) = tx_close.borrow_mut().take() {
            let _ = tx.send(Err("WebSocket closed before opening".into()));
        }
    }) as Box<dyn FnMut()>);

    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

    // Keep closures alive
    onopen.forget();
    onerror.forget();
    onclose.forget();

    rx.await.map_err(|_| "Channel closed".to_string())?
}

#[cfg_attr(not(target_arch = "wasm32"), matchbox_socket::async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", matchbox_socket::async_trait::async_trait(?Send))]
impl SignallerBuilder for YjsSignallerBuilder {
    async fn new_signaller(
        &self,
        _attempts: Option<u16>,
        room_url: String,
    ) -> Result<Box<dyn Signaller>, SignalingError> {
        let room_name = room_url.split('/').last().unwrap_or(&room_url).to_string();
        web_sys::console::log_1(&format!("[yjs] Creating signaller for room: {room_name}").into());

        // Try each signaling server
        for server_url in SIGNALING_SERVERS {
            web_sys::console::log_1(&format!("[yjs] Trying {server_url}...").into());

            // Create raw WebSocket first to properly wait for connection
            let raw_ws = match web_sys::WebSocket::new(server_url) {
                Ok(ws) => ws,
                Err(e) => {
                    web_sys::console::warn_1(&format!("[yjs] Failed to create WebSocket for {server_url}: {e:?}").into());
                    continue;
                }
            };

            // Wait for connection to be established
            match wait_for_open(&raw_ws).await {
                Ok(()) => {
                    web_sys::console::log_1(&format!("[yjs] Connected to {server_url}").into());
                }
                Err(e) => {
                    web_sys::console::warn_1(&format!("[yjs] Connection to {server_url} failed: {e}").into());
                    let _ = raw_ws.close();
                    continue;
                }
            }

            // Now wrap it with gloo-net for the nice async API
            // We need to create a new WebSocket since gloo-net takes ownership
            match WebSocket::open(server_url) {
                Ok(ws) => {
                    let our_id = uuid::Uuid::new_v4().to_string();
                    let matchbox_id = PeerId(uuid::Uuid::new_v4());

                    // Set up channels
                    let (req_tx, req_rx) = async_channel::bounded(64);
                    let (event_tx, event_rx) = async_channel::bounded(64);

                    // Send ID assigned
                    let _ = event_tx.send(PeerEvent::IdAssigned(matchbox_id)).await;

                    // Close the raw WebSocket we used for testing
                    let _ = raw_ws.close();

                    // Spawn the signaller task
                    spawn_local(run_signaller(
                        ws,
                        server_url.to_string(),
                        room_name.clone(),
                        our_id,
                        req_rx,
                        event_tx,
                    ));

                    return Ok(Box::new(YjsSignaller {
                        request_tx: req_tx,
                        event_rx,
                    }));
                }
                Err(e) => {
                    web_sys::console::warn_1(&format!("[yjs] Failed to open gloo WebSocket for {server_url}: {e:?}").into());
                    let _ = raw_ws.close();
                }
            }
        }

        Err(SignalingError::UserImplementationError("All signaling servers failed".into()))
    }
}

async fn run_signaller(
    ws: WebSocket,
    server_url: String,
    room: String,
    our_id: String,
    req_rx: async_channel::Receiver<PeerRequest>,
    event_tx: async_channel::Sender<PeerEvent>,
) {
    let (mut sink, mut stream) = ws.split();

    // Wait a moment for the connection to stabilize
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let window = web_sys::window().unwrap();
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 100);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;

    // Peer ID mappings: y-webrtc ID <-> matchbox PeerId
    let mut yjs_to_matchbox: HashMap<String, PeerId> = HashMap::new();
    let mut matchbox_to_yjs: HashMap<PeerId, String> = HashMap::new();
    // Track peers we've already sent NewPeer for
    let mut announced_peers: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Subscribe to room
    let subscribe = SignalMessage::Subscribe { topics: vec![room.clone()] };
    if let Ok(json) = serde_json::to_string(&subscribe) {
        web_sys::console::log_1(&format!("[yjs] Sending subscribe: {json}").into());
        if let Err(e) = sink.send(Message::Text(json)).await {
            web_sys::console::error_1(&format!("[yjs] Failed to send subscribe: {e:?}").into());
            return;
        }
    }

    // Announce ourselves
    let announce = SignalMessage::Publish {
        topic: room.clone(),
        data: SignalData {
            from: our_id.clone(),
            to: None,
            signal: SignalPayload::Announce,
        },
    };
    if let Ok(json) = serde_json::to_string(&announce) {
        web_sys::console::log_1(&format!("[yjs] Sending announce").into());
        if let Err(e) = sink.send(Message::Text(json)).await {
            web_sys::console::error_1(&format!("[yjs] Failed to send announce: {e:?}").into());
            return;
        }
    }

    web_sys::console::log_1(&format!("[yjs] Subscribed to room {room}, our ID: {our_id}").into());

    loop {
        use futures::future::Either;

        // Wait for either a matchbox request or a server message
        let either = futures::future::select(
            Box::pin(req_rx.recv()),
            Box::pin(stream.next()),
        ).await;

        match either {
            Either::Left((req_result, _)) => {
                match req_result {
                    Ok(req) => {
                        match req {
                            PeerRequest::Signal { receiver, data } => {
                                if let Some(target_id) = matchbox_to_yjs.get(&receiver) {
                                    let payload = match data {
                                        PeerSignal::Offer(sdp) => {
                                            web_sys::console::log_1(&format!("[yjs] Sending offer to {}", target_id).into());
                                            SignalPayload::Offer { sdp }
                                        }
                                        PeerSignal::Answer(sdp) => {
                                            web_sys::console::log_1(&format!("[yjs] Sending answer to {}", target_id).into());
                                            SignalPayload::Answer { sdp }
                                        }
                                        PeerSignal::IceCandidate(c) => {
                                            web_sys::console::log_1(&format!("[yjs] Sending ICE candidate to {}", target_id).into());
                                            SignalPayload::Candidate { candidate: c }
                                        }
                                    };

                                    let msg = SignalMessage::Publish {
                                        topic: room.clone(),
                                        data: SignalData {
                                            from: our_id.clone(),
                                            to: Some(target_id.clone()),
                                            signal: payload,
                                        },
                                    };

                                    if let Ok(json) = serde_json::to_string(&msg) {
                                        if let Err(e) = sink.send(Message::Text(json)).await {
                                            web_sys::console::error_1(&format!("[yjs] Failed to send signal: {e:?}").into());
                                        }
                                    }
                                } else {
                                    web_sys::console::warn_1(&format!("[yjs] Unknown receiver peer: {:?}", receiver).into());
                                }
                            }
                            PeerRequest::KeepAlive => {
                                // Re-announce periodically
                                let announce = SignalMessage::Publish {
                                    topic: room.clone(),
                                    data: SignalData {
                                        from: our_id.clone(),
                                        to: None,
                                        signal: SignalPayload::Announce,
                                    },
                                };
                                if let Ok(json) = serde_json::to_string(&announce) {
                                    let _ = sink.send(Message::Text(json)).await;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        web_sys::console::log_1(&"[yjs] Request channel closed".into());
                        return;
                    }
                }
            }
            Either::Right((ws_msg, _)) => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(msg) = serde_json::from_str::<ServerMessage>(&text) {
                            match msg {
                                ServerMessage::Ping => {
                                    let pong = SignalMessage::Pong;
                                    if let Ok(json) = serde_json::to_string(&pong) {
                                        let _ = sink.send(Message::Text(json)).await;
                                    }
                                }
                                ServerMessage::Pong => {}
                                ServerMessage::Publish { data, .. } => {
                                    // Ignore our own messages
                                    if data.from == our_id {
                                        continue;
                                    }

                                    // Ignore messages not for us
                                    if let Some(ref to) = data.to {
                                        if *to != our_id {
                                            continue;
                                        }
                                    }

                                    // Get or create matchbox peer ID
                                    let peer_id = *yjs_to_matchbox
                                        .entry(data.from.clone())
                                        .or_insert_with(|| {
                                            let id = PeerId(uuid::Uuid::new_v4());
                                            matchbox_to_yjs.insert(id, data.from.clone());
                                            id
                                        });

                                    match data.signal {
                                        SignalPayload::Announce => {
                                            if announced_peers.insert(data.from.clone()) {
                                                web_sys::console::log_1(&format!("[yjs] New peer: {}", data.from).into());
                                                let _ = event_tx.send(PeerEvent::NewPeer(peer_id)).await;
                                            }
                                        }
                                        SignalPayload::Offer { sdp } => {
                                            web_sys::console::log_1(&format!("[yjs] Received offer from {}", data.from).into());
                                            if announced_peers.insert(data.from.clone()) {
                                                let _ = event_tx.send(PeerEvent::NewPeer(peer_id)).await;
                                            }
                                            let _ = event_tx.send(PeerEvent::Signal {
                                                sender: peer_id,
                                                data: PeerSignal::Offer(sdp),
                                            }).await;
                                        }
                                        SignalPayload::Answer { sdp } => {
                                            web_sys::console::log_1(&format!("[yjs] Received answer from {}", data.from).into());
                                            let _ = event_tx.send(PeerEvent::Signal {
                                                sender: peer_id,
                                                data: PeerSignal::Answer(sdp),
                                            }).await;
                                        }
                                        SignalPayload::Candidate { candidate } => {
                                            web_sys::console::log_1(&format!("[yjs] Received ICE candidate from {}", data.from).into());
                                            let _ = event_tx.send(PeerEvent::Signal {
                                                sender: peer_id,
                                                data: PeerSignal::IceCandidate(candidate),
                                            }).await;
                                        }
                                    }
                                }
                                ServerMessage::Other => {}
                            }
                        }
                    }
                    Some(Ok(Message::Bytes(_))) => {}
                    Some(Err(e)) => {
                        web_sys::console::error_1(&format!("[yjs] WebSocket error: {e:?}").into());
                        return;
                    }
                    None => {
                        web_sys::console::log_1(&"[yjs] WebSocket closed".into());
                        return;
                    }
                }
            }
        }
    }
}

struct YjsSignaller {
    request_tx: async_channel::Sender<PeerRequest>,
    event_rx: async_channel::Receiver<PeerEvent>,
}

#[cfg_attr(not(target_arch = "wasm32"), matchbox_socket::async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", matchbox_socket::async_trait::async_trait(?Send))]
impl Signaller for YjsSignaller {
    async fn send(&mut self, request: PeerRequest) -> Result<(), SignalingError> {
        self.request_tx
            .send(request)
            .await
            .map_err(|e| SignalingError::UserImplementationError(e.to_string()))
    }

    async fn next_message(&mut self) -> Result<PeerEvent, SignalingError> {
        self.event_rx
            .recv()
            .await
            .map_err(|_| SignalingError::StreamExhausted)
    }
}
