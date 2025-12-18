//! P2P synchronization for yrs CRDT over matchbox WebRTC.
//!
//! Uses matchbox_server for signaling and matchbox for WebRTC data channels.
//! yrs updates are broadcast to all connected peers.

use futures_signals::signal::Mutable;
use matchbox_socket::{PeerId, PeerState, RtcIceServerConfig, WebRtcSocket};
use wasm_bindgen_futures::spawn_local;

use crate::room::{RoomState, YrsRoomState};

/// Matchbox signaling server
const SIGNALING_SERVER: &str = "wss://matchbox.wondering.xyz";

/// Manages P2P synchronization of room state.
pub struct RoomSync {
    socket: WebRtcSocket,
    room: Mutable<YrsRoomState>,
    room_version: Mutable<u64>,
}

impl RoomSync {
    /// Start P2P sync for the given room state.
    /// Returns a handle that keeps sync running.
    pub async fn start(
        room: Mutable<YrsRoomState>,
        room_topic: &str,
        peer_id_out: Mutable<Option<String>>,
        room_version: Mutable<u64>,
    ) -> Result<Self, String> {
        // Extract just the room name (strip any existing @peer-id suffix)
        let room_name = room_topic.split('@').next().unwrap_or(room_topic);

        web_sys::console::log_1(&format!(
            "[Sync] Starting sync for room: {room_name}"
        ).into());

        // Build WebRTC socket with native matchbox signaling + STUN/TURN for faster ICE
        let signaling_url = format!("{SIGNALING_SERVER}/{room_name}");
        let (socket, loop_fut) = WebRtcSocket::builder(&signaling_url)
            // Google STUN
            .ice_server(RtcIceServerConfig {
                urls: vec![
                    "stun:stun.l.google.com:19302".to_string(),
                    "stun:stun1.l.google.com:19302".to_string(),
                ],
                username: None,
                credential: None,
            })
            // Free TURN from metered.ca (for testing - get your own for production)
            .ice_server(RtcIceServerConfig {
                urls: vec![
                    "turn:a.relay.metered.ca:80".to_string(),
                    "turn:a.relay.metered.ca:443".to_string(),
                    "turn:a.relay.metered.ca:443?transport=tcp".to_string(),
                ],
                username: Some("e8dd65c92f8d9b7a3b74a5e0".to_string()),
                credential: Some("kFLwHSi+E3IgNnwd".to_string()),
            })
            .add_reliable_channel()
            .build();

        // Set a display peer ID
        peer_id_out.set(Some(format!("room:{room_name}")));

        // Spawn the socket message loop
        spawn_local(async move {
            let _ = loop_fut.await;
        });

        Ok(Self { socket, room, room_version })
    }

    /// Run the sync loop - call this in a spawned task.
    pub async fn run(mut self) {
        let mut peers: Vec<PeerId> = Vec::new();

        // Get notification receiver for local changes
        let mut notify_rx = self.room.lock_ref().subscribe();

        // Track the last state we broadcast (to compute diffs)
        let mut last_broadcast_sv = self.room.lock_ref().state_vector();

        loop {
            // Check for peer updates (non-blocking)
            for (peer_id, state) in self.socket.update_peers() {
                match state {
                    PeerState::Connected => {
                        web_sys::console::log_1(&format!("Peer connected: {peer_id}").into());
                        peers.push(peer_id);

                        // Send our current state to the new peer
                        let state_update = self.room.lock_ref().encode_state_as_update();
                        web_sys::console::log_1(&format!(
                            "Sending state update to {peer_id} ({} bytes)",
                            state_update.len()
                        ).into());
                        self.socket.channel_mut(0).send(state_update.into_boxed_slice(), peer_id);
                    }
                    PeerState::Disconnected => {
                        web_sys::console::log_1(&format!("Peer disconnected: {peer_id}").into());
                        peers.retain(|p| *p != peer_id);

                        // Remove peer from room state
                        self.room.lock_mut().remove_peer(&peer_id.0.to_string());
                    }
                }
            }

            // Handle incoming messages (yrs updates)
            // NOTE: apply_update() does NOT trigger notify(), so we won't
            // accidentally rebroadcast remote changes. This is handled in
            // YrsRoomState::apply_update() which intentionally skips notify().
            for (peer_id, data) in self.socket.channel_mut(0).receive() {
                web_sys::console::log_1(&format!(
                    "Received update from {peer_id} ({} bytes)",
                    data.len()
                ).into());
                if let Err(e) = self.room.lock_mut().apply_update(&data) {
                    web_sys::console::warn_1(&format!(
                        "Failed to apply update from {peer_id}: {e}"
                    ).into());
                } else {
                    web_sys::console::log_1(&format!(
                        "Successfully applied update from {peer_id}"
                    ).into());
                    // Update our state vector after applying remote changes
                    last_broadcast_sv = self.room.lock_ref().state_vector();
                    // Increment room version to trigger UI updates
                    self.room_version.set(self.room_version.get() + 1);
                }
            }

            // Check for local changes and broadcast them
            // NOTE: Only local changes trigger has_changed() because apply_update()
            // intentionally does NOT call notify(). This prevents feedback loops.
            if notify_rx.has_changed().unwrap_or(false) {
                web_sys::console::log_1(&"[Sync] Detected local change via notify".into());
                let _ = notify_rx.borrow_and_update(); // Clear the flag

                // Get update since last broadcast
                let room = self.room.lock_ref();
                if let Ok(update) = room.encode_diff(&last_broadcast_sv) {
                    web_sys::console::log_1(&format!(
                        "[Sync] Diff computed: {} bytes, peers: {}",
                        update.len(),
                        peers.len()
                    ).into());
                    if !update.is_empty() && !peers.is_empty() {
                        web_sys::console::log_1(&format!(
                            "[Sync] Broadcasting local update ({} bytes) to {} peers",
                            update.len(),
                            peers.len()
                        ).into());

                        for peer_id in &peers {
                            self.socket.channel_mut(0).send(update.clone().into_boxed_slice(), *peer_id);
                        }

                        // Update our tracked state vector
                        last_broadcast_sv = room.state_vector();
                    } else if update.is_empty() {
                        web_sys::console::log_1(&"[Sync] Diff was empty, nothing to broadcast".into());
                    } else {
                        web_sys::console::log_1(&"[Sync] No peers connected, can't broadcast".into());
                    }
                } else {
                    web_sys::console::warn_1(&"[Sync] Failed to compute diff".into());
                }
            }

            // Yield to other tasks
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                let window = web_sys::window().unwrap();
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 16);
            });
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }
    }

    /// Broadcast a yrs update to all connected peers.
    pub fn broadcast_update(&mut self, update: &[u8]) {
        let peers: Vec<PeerId> = self.socket.connected_peers().collect();
        for peer_id in peers {
            self.socket.channel_mut(0).send(update.to_vec().into_boxed_slice(), peer_id);
        }
    }
}

/// Start room sync in the background.
/// Returns when sync is initialized (but continues running).
/// The `iroh_peer_id` Mutable will be set with our peer ID once connected.
/// The `room_version` will be incremented when remote updates are received.
pub fn start_room_sync(
    room: Mutable<YrsRoomState>,
    room_topic: String,
    iroh_peer_id: Mutable<Option<String>>,
    room_version: Mutable<u64>,
) {
    spawn_local(async move {
        match RoomSync::start(room, &room_topic, iroh_peer_id, room_version).await {
            Ok(sync) => {
                web_sys::console::log_1(&"Room sync started".into());
                sync.run().await;
            }
            Err(e) => {
                web_sys::console::error_1(&format!("Room sync failed: {e}").into());
            }
        }
    });
}
