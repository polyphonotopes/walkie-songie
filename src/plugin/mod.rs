//! nih-plug VST3/CLAP plugin for walkie-songie.
//!
//! Enables DAW users to join walkie-songie channels and collaborate
//! with mobile/web users via P2P using the same matchbox + yrs stack.

mod editor;
mod params;

// File-based logging for debugging (tail -f /tmp/walkie-songie.log)
macro_rules! plog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/walkie-songie.log")
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "[{}] {}", now, format!($($arg)*));
        }
    }};
}

use std::sync::{Arc, Mutex, RwLock};

use crossbeam_channel::{Receiver, Sender};
use nih_plug::prelude::*;

use crate::room::{
    PitchClassDelta, PitchDelta, RoomState, piece_pitch_deltas, unified_pitch_class_deltas,
    voice_pitch_deltas,
};
use crate::tuning::PitchClass;
use crate::words::generate_room_name;

pub use editor::WalkieSongieEditor;
pub use params::WalkieSongieParams;

/// libp2p relay server (same as web app)
const RELAY_ADDR: &str = "/dns4/libp2p.wondering.xyz/tcp/443/wss";

/// Messages from the plugin to the networking thread.
#[derive(Debug, Clone)]
pub enum NetCommand {
    /// Connect to a new channel
    JoinChannel(String),
    /// Set a pitch class on/off locally (for the room PCS)
    SetPitchClass { pitch_class: u8, on: bool },
    /// Set voice pitch (absolute pitch number, like MIDI note)
    SetVoicePitch(Option<i32>),
    /// Shutdown the networking thread
    Shutdown,
}

/// Messages from the networking thread to the plugin.
///
/// Uses pre-computed deltas from shared streams - the plugin just outputs MIDI directly.
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// Connection status changed
    ConnectionStatus { connected_peers: usize },
    /// Our peer ID is now known
    PeerIdAssigned(String),
    /// Unified pitch class delta (channel 0) - from ALL sources
    UnifiedPitchClassDelta(PitchClassDelta),
    /// Piece pitch delta (channel 2) - absolute pitches with octave info
    PiecePitchDelta(PitchDelta),
    /// Voice pitch delta (channel 1) - absolute pitches for melody
    VoicePitchDelta(PitchDelta),
    /// Error occurred
    Error(String),
}

// MIDI channels are now configurable via params (1-16 displayed, 0-15 internal)

/// The walkie-songie plugin.
///
/// Uses pre-computed deltas from shared streams - no shadow state needed.
/// MIDI output is directly derived from delta events.
pub struct WalkieSongiePlugin {
    params: Arc<WalkieSongieParams>,

    /// Channel to send commands to the networking thread
    net_tx: Option<Sender<NetCommand>>,
    /// Channel to receive events from the networking thread
    net_rx: Option<Receiver<NetEvent>>,

    /// Current connection status for UI
    connected_peers: Arc<Mutex<usize>>,
    /// Our peer ID (for sharing)
    peer_id: Arc<Mutex<Option<String>>>,

    /// Local pitch classes (what we're contributing to the room via MIDI input)
    local_pitch_classes: [bool; 128],
    /// Current local voice pitch (from MIDI input)
    local_voice_pitch: Option<i32>,
}

impl Default for WalkieSongiePlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WalkieSongieParams::default()),
            net_tx: None,
            net_rx: None,
            connected_peers: Arc::new(Mutex::new(0)),
            peer_id: Arc::new(Mutex::new(None)),
            local_pitch_classes: [false; 128],
            local_voice_pitch: None,
        }
    }
}

impl Plugin for WalkieSongiePlugin {
    const NAME: &'static str = "Walkie Songie";
    const VENDOR: &'static str = "@micahscopes";
    const URL: &'static str = "https://polyphonotopes.github.io/walkie-songie";
    const EMAIL: &'static str = "micahscopes@gmail.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // No audio I/O - this is a MIDI utility plugin
        AudioIOLayout {
            main_input_channels: None,
            main_output_channels: None,
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;

    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = NetCommand;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        Some(Box::new(WalkieSongieEditor::new(
            self.params.clone(),
            self.connected_peers.clone(),
            self.peer_id.clone(),
            self.net_tx.clone(),
        )))
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        // Create channels for networking thread communication
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (evt_tx, evt_rx) = crossbeam_channel::unbounded();

        self.net_tx = Some(cmd_tx);
        self.net_rx = Some(evt_rx);

        // Spawn networking thread
        let connected_peers = self.connected_peers.clone();
        let initial_channel = self.params.channel_address.lock().unwrap().clone();

        std::thread::spawn(move || {
            run_networking_thread(cmd_rx, evt_tx, connected_peers, initial_channel);
        });

        true
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Process incoming MIDI
        // Uses the same channel config as output for consistency
        let pc_channel = (self.params.pitch_classes_channel.value() - 1) as u8;
        let voice_channel = (self.params.voice_channel.value() - 1) as u8;

        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOn { note, channel, .. } => {
                    if channel == pc_channel {
                        // Pitch class input (toggle on)
                        let pc = note % 12;
                        if !self.local_pitch_classes[pc as usize] {
                            self.local_pitch_classes[pc as usize] = true;
                            if let Some(tx) = &self.net_tx {
                                let _ = tx.try_send(NetCommand::SetPitchClass {
                                    pitch_class: pc,
                                    on: true,
                                });
                            }
                        }
                    } else if channel == voice_channel {
                        // Voice pitch input (monophonic - new note replaces old)
                        self.local_voice_pitch = Some(note as i32);
                        if let Some(tx) = &self.net_tx {
                            let _ = tx.try_send(NetCommand::SetVoicePitch(Some(note as i32)));
                        }
                    }
                }
                NoteEvent::NoteOff { note, channel, .. } => {
                    if channel == pc_channel {
                        // Pitch class input (toggle off)
                        let pc = note % 12;
                        if self.local_pitch_classes[pc as usize] {
                            self.local_pitch_classes[pc as usize] = false;
                            if let Some(tx) = &self.net_tx {
                                let _ = tx.try_send(NetCommand::SetPitchClass {
                                    pitch_class: pc,
                                    on: false,
                                });
                            }
                        }
                    } else if channel == voice_channel {
                        // Voice pitch off (only if it matches current voice)
                        if self.local_voice_pitch == Some(note as i32) {
                            self.local_voice_pitch = None;
                            if let Some(tx) = &self.net_tx {
                                let _ = tx.try_send(NetCommand::SetVoicePitch(None));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Process delta events from networking thread (non-blocking)
        // Deltas are pre-computed by shared streams - just output MIDI directly
        if let Some(rx) = &self.net_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    NetEvent::ConnectionStatus { connected_peers } => {
                        if let Ok(mut count) = self.connected_peers.lock() {
                            *count = connected_peers;
                        }
                    }
                    NetEvent::PeerIdAssigned(peer_id) => {
                        if let Ok(mut pid) = self.peer_id.lock() {
                            *pid = Some(peer_id);
                        }
                    }
                    NetEvent::UnifiedPitchClassDelta(delta) => {
                        if self.params.pitch_classes_enabled.value() {
                            let channel = (self.params.pitch_classes_channel.value() - 1) as u8;
                            for pc in delta.added {
                                if pc < 12 {
                                    context.send_event(NoteEvent::NoteOn {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pc + 60,
                                        velocity: 0.8,
                                    });
                                }
                            }
                            for pc in delta.removed {
                                if pc < 12 {
                                    context.send_event(NoteEvent::NoteOff {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pc + 60,
                                        velocity: 0.0,
                                    });
                                }
                            }
                        }
                    }
                    NetEvent::VoicePitchDelta(delta) => {
                        if self.params.voice_enabled.value() {
                            let channel = (self.params.voice_channel.value() - 1) as u8;
                            for pitch in delta.added {
                                if pitch >= 0 && pitch < 128 {
                                    context.send_event(NoteEvent::NoteOn {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pitch as u8,
                                        velocity: 0.8,
                                    });
                                }
                            }
                            for pitch in delta.removed {
                                if pitch >= 0 && pitch < 128 {
                                    context.send_event(NoteEvent::NoteOff {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pitch as u8,
                                        velocity: 0.0,
                                    });
                                }
                            }
                        }
                    }
                    NetEvent::PiecePitchDelta(delta) => {
                        if self.params.pieces_enabled.value() {
                            let channel = (self.params.pieces_channel.value() - 1) as u8;
                            for pitch in delta.added {
                                if pitch >= 0 && pitch < 128 {
                                    context.send_event(NoteEvent::NoteOn {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pitch as u8,
                                        velocity: 0.8,
                                    });
                                }
                            }
                            for pitch in delta.removed {
                                if pitch >= 0 && pitch < 128 {
                                    context.send_event(NoteEvent::NoteOff {
                                        timing: 0,
                                        voice_id: None,
                                        channel,
                                        note: pitch as u8,
                                        velocity: 0.0,
                                    });
                                }
                            }
                        }
                    }
                    NetEvent::Error(msg) => {
                        nih_warn!("Network error: {}", msg);
                    }
                }
            }
        }

        ProcessStatus::Normal
    }

    fn deactivate(&mut self) {
        // Signal networking thread to shutdown
        if let Some(tx) = &self.net_tx {
            let _ = tx.try_send(NetCommand::Shutdown);
        }
    }
}

impl ClapPlugin for WalkieSongiePlugin {
    const CLAP_ID: &'static str = "xyz.wondering.walkie-songie";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("P2P collaborative music - join channels with QR codes");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Utility, ClapFeature::NoteEffect];
}

impl Vst3Plugin for WalkieSongiePlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"walkiesongierust";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Tools, Vst3SubCategory::Fx];
}

/// Networking thread - handles all P2P communication off the audio thread.
fn run_networking_thread(
    cmd_rx: Receiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
    _connected_peers: Arc<Mutex<usize>>,
    initial_channel: String,
) {
    // Create a tokio runtime for async networking
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = evt_tx.send(NetEvent::Error(format!("Failed to create runtime: {}", e)));
            return;
        }
    };

    rt.block_on(async {
        if let Err(e) = run_networking_loop(cmd_rx, evt_tx.clone(), initial_channel).await {
            nih_warn!("Networking error: {}", e);
            let _ = evt_tx.send(NetEvent::Error(format!("{}", e)));
        }
    });
}

/// Main networking loop using libp2p + yrs (same protocol as web app).
async fn run_networking_loop(
    cmd_rx: Receiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
    initial_channel: String,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    use libp2p::{
        Multiaddr, SwarmBuilder,
        gossipsub::{self, IdentTopic, MessageAuthenticity},
        identify,
        swarm::{NetworkBehaviour, SwarmEvent},
    };

    plog!("=== Networking loop starting (libp2p) ===");

    let initial_channel = if initial_channel.is_empty() {
        plog!("No channel provided, generating room name");
        generate_room_name()
    } else {
        initial_channel
    };

    // Extract just the room name (strip any existing @peer-id suffix)
    let room_name = initial_channel
        .split('@')
        .next()
        .unwrap_or(&initial_channel);
    let topic = IdentTopic::new(format!("walkie-songie/{}", room_name));

    plog!("Room name: {}, topic: {}", room_name, topic);
    nih_log!("Initializing libp2p connection to room: {}", room_name);

    // Build libp2p swarm with gossipsub
    #[derive(NetworkBehaviour)]
    struct Behaviour {
        gossipsub: gossipsub::Behaviour,
        identify: identify::Behaviour,
    }

    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_websocket(
            (libp2p::tls::Config::new, libp2p::noise::Config::new),
            libp2p::yamux::Config::default,
        )
        .await?
        .with_behaviour(|key| {
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(std::time::Duration::from_secs(1))
                .validation_mode(gossipsub::ValidationMode::Permissive)
                .build()
                .expect("valid gossipsub config");

            let gossipsub = gossipsub::Behaviour::new(
                MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .expect("valid gossipsub behaviour");

            let identify = identify::Behaviour::new(identify::Config::new(
                "/walkie-songie/1.0.0".to_string(),
                key.public(),
            ));

            Behaviour {
                gossipsub,
                identify,
            }
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(60)))
        .build();

    let local_peer_id = *swarm.local_peer_id();
    plog!("Local peer ID: {}", local_peer_id);
    nih_log!("Local peer ID: {}", local_peer_id);

    // Subscribe to topic
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
    plog!("Subscribed to topic: {}", topic);

    // Connect to relay
    let relay_addr: Multiaddr = RELAY_ADDR.parse()?;
    plog!("Dialing relay: {}", relay_addr);
    swarm.dial(relay_addr.clone())?;

    // Create CRDT peer ID and room state (wrapped in Arc<Mutex<>> for stream access)
    let crdt_peer_id = format!("peer-{}", uuid::Uuid::new_v4());
    let _ = evt_tx.send(NetEvent::PeerIdAssigned(format!(
        "{}@{}",
        room_name,
        &crdt_peer_id[5..13]
    )));

    plog!("Creating RoomState with peer_id: {}", crdt_peer_id);
    let room = Arc::new(RwLock::new(RoomState::new(crdt_peer_id.clone())));

    // Subscribe to delta streams for MIDI output
    // These query authoritative CRDT state on each event
    let mut pitch_class_deltas = std::pin::pin!(unified_pitch_class_deltas(room.clone()));
    let mut piece_deltas = std::pin::pin!(piece_pitch_deltas(room.clone()));
    let mut voice_deltas = std::pin::pin!(voice_pitch_deltas(room.clone()));

    // Track state
    let mut last_broadcast_sv = room.read().unwrap().state_vector();
    let mut last_known_peer_count = room.read().unwrap().all_peer_sets().len();
    let mut has_local_changes = false;
    let mut connected_to_relay = false;

    let mut loop_count: u64 = 0;
    let loop_start = std::time::Instant::now();

    plog!("Entering main loop...");

    loop {
        loop_count += 1;
        if loop_count % 312 == 0 {
            plog!(
                "Heartbeat: {} loops, {} crdt_peers, connected={}, uptime {:?}",
                loop_count,
                room.read().unwrap().all_peer_sets().len(),
                connected_to_relay,
                loop_start.elapsed()
            );
        }

        // Process commands from plugin (non-blocking)
        match cmd_rx.try_recv() {
            Ok(NetCommand::Shutdown) => {
                plog!("Shutdown requested");
                return Ok(());
            }
            Ok(NetCommand::JoinChannel(new_channel)) => {
                plog!(
                    "Channel switch requested to {} (not implemented)",
                    new_channel
                );
            }
            Ok(NetCommand::SetPitchClass { pitch_class, on }) => {
                plog!("Local change: SetPitchClass {} = {}", pitch_class, on);
                let mut room_guard = room.write().unwrap();
                if on {
                    room_guard.add_pitch(PitchClass(pitch_class));
                } else {
                    room_guard.remove_pitch(PitchClass(pitch_class));
                }
                has_local_changes = true;
            }
            Ok(NetCommand::SetVoicePitch(pitch)) => {
                plog!("Local change: SetVoicePitch {:?}", pitch);
                let mut room_guard = room.write().unwrap();
                room_guard.set_voice_pitch(pitch);
                if let Some(p) = pitch {
                    room_guard.set_voice_pitchclass(Some(PitchClass((p % 12) as u8)));
                } else {
                    room_guard.set_voice_pitchclass(None);
                }
                has_local_changes = true;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                plog!("Command channel disconnected");
                return Ok(());
            }
        }

        // Process swarm events with timeout
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(16));
        tokio::pin!(timeout);

        tokio::select! {
            // Forward unified pitch class deltas to plugin
            delta = pitch_class_deltas.next() => {
                if let Some(d) = delta {
                    plog!("PitchClassDelta: +{:?} -{:?}", d.added, d.removed);
                    let _ = evt_tx.send(NetEvent::UnifiedPitchClassDelta(d));
                }
            }

            // Forward piece pitch deltas to plugin
            delta = piece_deltas.next() => {
                if let Some(d) = delta {
                    plog!("PieceDelta: +{:?} -{:?}", d.added, d.removed);
                    let _ = evt_tx.send(NetEvent::PiecePitchDelta(d));
                }
            }

            // Forward voice pitch deltas to plugin
            delta = voice_deltas.next() => {
                if let Some(d) = delta {
                    plog!("VoiceDelta: +{:?} -{:?}", d.added, d.removed);
                    let _ = evt_tx.send(NetEvent::VoicePitchDelta(d));
                }
            }

            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        plog!("Connected to peer: {}", peer_id);
                        if !connected_to_relay {
                            connected_to_relay = true;
                            let _ = evt_tx.send(NetEvent::ConnectionStatus { connected_peers: 1 });
                        }
                    }
                    SwarmEvent::ConnectionClosed { peer_id, .. } => {
                        plog!("Disconnected from peer: {}", peer_id);
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        propagation_source,
                        message,
                        ..
                    })) => {
                        plog!("Received gossipsub message from {} ({} bytes)",
                            propagation_source, message.data.len());

                        // apply_update now emits RoomEvents which will be handled by delta streams
                        let mut room_guard = room.write().unwrap();
                        if let Err(e) = room_guard.apply_update(&message.data) {
                            plog!("Failed to apply update: {}", e);
                        } else {
                            last_broadcast_sv = room_guard.state_vector();
                        }
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed {
                        peer_id,
                        topic: t,
                    })) => {
                        plog!("Peer {} subscribed to {}", peer_id, t);
                        // Send full state when peer subscribes to OUR topic
                        if t == topic.hash() {
                            let room_guard = room.read().unwrap();
                            let state_update = room_guard.encode_state_as_update();
                            plog!("Sending full state to new subscriber ({} bytes)", state_update.len());
                            if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), state_update) {
                                plog!("Failed to send state to subscriber: {:?}", e);
                            }
                            // Emit FullStateSync for ourselves too
                            room_guard.emit_full_state_sync();
                        }
                    }
                    _ => {}
                }
            }
            _ = &mut timeout => {
                // Check for new peers (triggered by receiving their state)
                let current_peer_count = room.read().unwrap().all_peer_sets().len();
                let new_peer_joined = current_peer_count > last_known_peer_count;
                if new_peer_joined {
                    plog!("New peer detected ({} -> {}), sending full state", last_known_peer_count, current_peer_count);
                    last_known_peer_count = current_peer_count;

                    // Send full state to new peer immediately
                    if connected_to_relay {
                        let room_guard = room.read().unwrap();
                        let state_update = room_guard.encode_state_as_update();
                        if !state_update.is_empty() {
                            plog!("Broadcasting FULL state ({} bytes)", state_update.len());
                            if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), state_update) {
                                plog!("Failed to publish full state: {:?}", e);
                            }
                            last_broadcast_sv = room_guard.state_vector();
                        }
                    }
                }

                // Send diff for local changes
                if has_local_changes && connected_to_relay {
                    let room_guard = room.read().unwrap();
                    let update = room_guard.encode_diff(&last_broadcast_sv).unwrap_or_default();

                    if !update.is_empty() {
                        plog!("Broadcasting diff ({} bytes)", update.len());
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), update) {
                            plog!("Failed to publish: {:?}", e);
                        }
                        last_broadcast_sv = room_guard.state_vector();
                    }
                    has_local_changes = false;
                }
            }
        }
    }
}

// Plugin exports are in lib.rs at crate root
