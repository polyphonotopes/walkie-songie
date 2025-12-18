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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use nih_plug::prelude::*;

use crate::room::{RoomState, YrsRoomState};
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
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// Connection status changed
    ConnectionStatus { connected_peers: usize },
    /// Our peer ID is now known
    PeerIdAssigned(String),
    /// Remote peer changed a pitch class (for room PCS)
    RemotePitchClassChange { peer_id: String, pitch_class: u8, on: bool },
    /// Remote peer changed their voice pitch
    RemoteVoicePitchChange { peer_id: String, pitch: Option<i32> },
    /// Error occurred
    Error(String),
}

/// MIDI channels for output
const CHANNEL_ROOM_PCS: u8 = 0;    // Channel 1 in DAW (0-indexed)
const CHANNEL_VOICE: u8 = 1;       // Channel 2 in DAW

/// The walkie-songie plugin.
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

    /// Local pitch classes (what we're contributing to the room PCS)
    local_pitch_classes: [bool; 128],
    /// Current local voice pitch
    local_voice_pitch: Option<i32>,

    /// Per-peer pitch class sets (for computing room PCS)
    peer_pitch_classes: HashMap<String, [bool; 128]>,
    /// Per-peer voice pitches
    peer_voice_pitches: HashMap<String, Option<i32>>,

    /// Currently active room PCS notes (for diffing)
    room_pcs_output: [bool; 128],
    /// Currently active voice notes (for diffing)
    voice_output: [bool; 128],
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
            peer_pitch_classes: HashMap::new(),
            peer_voice_pitches: HashMap::new(),
            room_pcs_output: [false; 128],
            voice_output: [false; 128],
        }
    }
}

impl Plugin for WalkieSongiePlugin {
    const NAME: &'static str = "Walkie Songie";
    const VENDOR: &'static str = "@micahscopes";
    const URL: &'static str = "https://wondering.xyz";
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
        // Channel 1 (0): Pitch class input → room PCS contribution
        // Channel 2 (1): Voice pitch input → voice state
        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOn { note, channel, .. } => {
                    if channel == CHANNEL_ROOM_PCS {
                        // Pitch class input (toggle on)
                        let pc = note % 12;
                        if !self.local_pitch_classes[pc as usize] {
                            self.local_pitch_classes[pc as usize] = true;
                            if let Some(tx) = &self.net_tx {
                                let _ = tx.try_send(NetCommand::SetPitchClass { pitch_class: pc, on: true });
                            }
                        }
                    } else if channel == CHANNEL_VOICE {
                        // Voice pitch input (monophonic - new note replaces old)
                        self.local_voice_pitch = Some(note as i32);
                        if let Some(tx) = &self.net_tx {
                            let _ = tx.try_send(NetCommand::SetVoicePitch(Some(note as i32)));
                        }
                    }
                }
                NoteEvent::NoteOff { note, channel, .. } => {
                    if channel == CHANNEL_ROOM_PCS {
                        // Pitch class input (toggle off)
                        let pc = note % 12;
                        if self.local_pitch_classes[pc as usize] {
                            self.local_pitch_classes[pc as usize] = false;
                            if let Some(tx) = &self.net_tx {
                                let _ = tx.try_send(NetCommand::SetPitchClass { pitch_class: pc, on: false });
                            }
                        }
                    } else if channel == CHANNEL_VOICE {
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

        // Process events from networking thread (non-blocking)
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
                    NetEvent::RemotePitchClassChange { peer_id, pitch_class, on } => {
                        // Update peer's pitch class set
                        let peer_set = self.peer_pitch_classes.entry(peer_id).or_insert([false; 128]);
                        peer_set[pitch_class as usize] = on;
                    }
                    NetEvent::RemoteVoicePitchChange { peer_id, pitch } => {
                        // Update peer's voice pitch
                        self.peer_voice_pitches.insert(peer_id, pitch);
                    }
                    NetEvent::Error(msg) => {
                        nih_warn!("Network error: {}", msg);
                    }
                }
            }
        }

        // Compute room PCS (union of all pitch classes including local)
        let mut room_pcs = [false; 128];
        for pc in 0..12u8 {
            if self.local_pitch_classes[pc as usize] {
                room_pcs[pc as usize] = true;
            }
            for peer_set in self.peer_pitch_classes.values() {
                if peer_set[pc as usize] {
                    room_pcs[pc as usize] = true;
                }
            }
        }

        // Output room PCS changes on channel 1
        for pc in 0..12u8 {
            let is_on = room_pcs[pc as usize];
            let was_on = self.room_pcs_output[pc as usize];
            if is_on && !was_on {
                context.send_event(NoteEvent::NoteOn {
                    timing: 0,
                    voice_id: None,
                    channel: CHANNEL_ROOM_PCS,
                    note: pc + 60, // Middle C octave
                    velocity: 0.8,
                });
            } else if !is_on && was_on {
                context.send_event(NoteEvent::NoteOff {
                    timing: 0,
                    voice_id: None,
                    channel: CHANNEL_ROOM_PCS,
                    note: pc + 60,
                    velocity: 0.0,
                });
            }
        }
        self.room_pcs_output = room_pcs;

        // Compute all voice pitches (including local)
        let mut voice_pitches = [false; 128];
        if let Some(pitch) = self.local_voice_pitch {
            if pitch >= 0 && pitch < 128 {
                voice_pitches[pitch as usize] = true;
            }
        }
        for pitch_opt in self.peer_voice_pitches.values() {
            if let Some(pitch) = pitch_opt {
                if *pitch >= 0 && *pitch < 128 {
                    voice_pitches[*pitch as usize] = true;
                }
            }
        }

        // Output voice pitch changes on channel 2
        for note in 0..128u8 {
            let is_on = voice_pitches[note as usize];
            let was_on = self.voice_output[note as usize];
            if is_on && !was_on {
                context.send_event(NoteEvent::NoteOn {
                    timing: 0,
                    voice_id: None,
                    channel: CHANNEL_VOICE,
                    note,
                    velocity: 0.8,
                });
            } else if !is_on && was_on {
                context.send_event(NoteEvent::NoteOff {
                    timing: 0,
                    voice_id: None,
                    channel: CHANNEL_VOICE,
                    note,
                    velocity: 0.0,
                });
            }
        }
        self.voice_output = voice_pitches;

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
    const CLAP_DESCRIPTION: Option<&'static str> = Some("P2P collaborative music - join channels with QR codes");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Utility,
        ClapFeature::NoteEffect,
    ];
}

impl Vst3Plugin for WalkieSongiePlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"walkiesongierust";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Tools,
        Vst3SubCategory::Fx,
    ];
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
    use libp2p::{
        gossipsub::{self, IdentTopic, MessageAuthenticity},
        identify,
        swarm::{SwarmEvent, NetworkBehaviour},
        Multiaddr, SwarmBuilder,
    };
    use futures::StreamExt;

    plog!("=== Networking loop starting (libp2p) ===");

    let initial_channel = if initial_channel.is_empty() {
        plog!("No channel provided, generating room name");
        generate_room_name()
    } else {
        initial_channel
    };

    // Extract just the room name (strip any existing @peer-id suffix)
    let room_name = initial_channel.split('@').next().unwrap_or(&initial_channel);
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

            Behaviour { gossipsub, identify }
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

    // Create CRDT peer ID and room state
    let crdt_peer_id = format!("peer-{}", uuid::Uuid::new_v4());
    let _ = evt_tx.send(NetEvent::PeerIdAssigned(format!("{}@{}", room_name, &crdt_peer_id[5..13])));

    plog!("Creating YrsRoomState with peer_id: {}", crdt_peer_id);
    let mut room = YrsRoomState::new(crdt_peer_id.clone());

    // Track state
    let mut last_broadcast_sv = room.state_vector();
    let mut last_known_peer_count = room.all_peer_sets().len();
    let mut has_local_changes = false;
    let mut connected_to_relay = false;

    // Track previous state for MIDI event diffing
    let mut prev_peer_sets: HashMap<String, Vec<u8>> = HashMap::new();
    let mut prev_voice_states: HashMap<String, (Option<i32>, Option<PitchClass>)> = HashMap::new();

    let mut loop_count: u64 = 0;
    let loop_start = std::time::Instant::now();

    plog!("Entering main loop...");

    loop {
        loop_count += 1;
        if loop_count % 312 == 0 {
            plog!("Heartbeat: {} loops, {} crdt_peers, connected={}, uptime {:?}",
                loop_count, room.all_peer_sets().len(), connected_to_relay, loop_start.elapsed());
        }

        // Process commands from plugin (non-blocking)
        match cmd_rx.try_recv() {
            Ok(NetCommand::Shutdown) => {
                plog!("Shutdown requested");
                return Ok(());
            }
            Ok(NetCommand::JoinChannel(new_channel)) => {
                plog!("Channel switch requested to {} (not implemented)", new_channel);
            }
            Ok(NetCommand::SetPitchClass { pitch_class, on }) => {
                plog!("Local change: SetPitchClass {} = {}", pitch_class, on);
                if on {
                    room.add_pitch(PitchClass(pitch_class));
                } else {
                    room.remove_pitch(PitchClass(pitch_class));
                }
                has_local_changes = true;
            }
            Ok(NetCommand::SetVoicePitch(pitch)) => {
                plog!("Local change: SetVoicePitch {:?}", pitch);
                room.set_voice_pitch(pitch);
                if let Some(p) = pitch {
                    room.set_voice_pitchclass(Some(PitchClass((p % 12) as u8)));
                } else {
                    room.set_voice_pitchclass(None);
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

                        if let Err(e) = room.apply_update(&message.data) {
                            plog!("Failed to apply update: {}", e);
                        } else {
                            last_broadcast_sv = room.state_vector();

                            // Notify plugin of remote changes
                            let peer_sets = room.all_peer_sets();
                            for (remote_peer_id, peer_set) in &peer_sets {
                                if remote_peer_id == &crdt_peer_id {
                                    continue;
                                }

                                let current_pcs: Vec<u8> = peer_set.pitch_classes.iter().map(|pc| pc.0).collect();
                                let prev_pcs = prev_peer_sets.get(remote_peer_id).cloned().unwrap_or_default();

                                for &pc in &current_pcs {
                                    if !prev_pcs.contains(&pc) {
                                        let _ = evt_tx.send(NetEvent::RemotePitchClassChange {
                                            peer_id: remote_peer_id.clone(),
                                            pitch_class: pc,
                                            on: true,
                                        });
                                    }
                                }
                                for &pc in &prev_pcs {
                                    if !current_pcs.contains(&pc) {
                                        let _ = evt_tx.send(NetEvent::RemotePitchClassChange {
                                            peer_id: remote_peer_id.clone(),
                                            pitch_class: pc,
                                            on: false,
                                        });
                                    }
                                }
                                prev_peer_sets.insert(remote_peer_id.clone(), current_pcs);
                            }

                            // Check voice state changes
                            let voice_states = room.all_voice_states();
                            for (remote_peer_id, (pitch, pc)) in &voice_states {
                                if remote_peer_id == &crdt_peer_id {
                                    continue;
                                }
                                let prev = prev_voice_states.get(remote_peer_id).cloned();
                                if prev.map(|(p, _)| p) != Some(*pitch) {
                                    let _ = evt_tx.send(NetEvent::RemoteVoicePitchChange {
                                        peer_id: remote_peer_id.clone(),
                                        pitch: *pitch,
                                    });
                                }
                                prev_voice_states.insert(remote_peer_id.clone(), (*pitch, *pc));
                            }
                        }
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed {
                        peer_id,
                        topic: t,
                    })) => {
                        plog!("Peer {} subscribed to {}", peer_id, t);
                        // Send full state to new subscriber
                        let state_update = room.encode_state_as_update();
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), state_update) {
                            plog!("Failed to send state to subscriber: {:?}", e);
                        }
                    }
                    _ => {}
                }
            }
            _ = &mut timeout => {
                // Check for new peers and broadcast local changes
                let current_peer_count = room.all_peer_sets().len();
                let new_peer_joined = current_peer_count > last_known_peer_count;
                if new_peer_joined {
                    plog!("New peer detected ({} -> {})", last_known_peer_count, current_peer_count);
                    last_known_peer_count = current_peer_count;
                }

                if has_local_changes && connected_to_relay {
                    let update = if new_peer_joined {
                        plog!("Broadcasting FULL state for new peer");
                        room.encode_state_as_update()
                    } else {
                        room.encode_diff(&last_broadcast_sv).unwrap_or_default()
                    };

                    if !update.is_empty() {
                        plog!("Broadcasting {} ({} bytes)",
                            if new_peer_joined { "FULL state" } else { "diff" },
                            update.len());
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), update) {
                            plog!("Failed to publish: {:?}", e);
                        }
                        last_broadcast_sv = room.state_vector();
                    }
                    has_local_changes = false;
                }
            }
        }
    }
}

// Plugin exports are in lib.rs at crate root
