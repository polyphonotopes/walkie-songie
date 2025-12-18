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

/// Matchbox signaling server (same as web app)
const SIGNALING_SERVER: &str = "wss://matchbox.wondering.xyz";

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

/// Main networking loop using matchbox + yrs (same as web app).
async fn run_networking_loop(
    cmd_rx: Receiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
    initial_channel: String,
) -> anyhow::Result<()> {
    use matchbox_socket::{PeerState, WebRtcSocket};

    plog!("=== Networking loop starting ===");

    let initial_channel = if initial_channel.is_empty() {
        plog!("No channel provided, generating room name");
        generate_room_name()
    } else {
        initial_channel
    };

    // Extract just the room name (strip any existing @peer-id suffix)
    let room_name = initial_channel.split('@').next().unwrap_or(&initial_channel);

    plog!("Room name: {}", room_name);
    nih_log!("Initializing matchbox connection to room: {}", room_name);

    // Build WebRTC socket with matchbox signaling (same server as web)
    let signaling_url = format!("{}/{}", SIGNALING_SERVER, room_name);
    plog!("Signaling URL: {}", signaling_url);
    plog!("Building WebRTC socket...");

    let build_start = std::time::Instant::now();
    let (mut socket, loop_fut) = WebRtcSocket::builder(&signaling_url)
        .add_reliable_channel()
        .build();
    plog!("WebRTC socket built in {:?}", build_start.elapsed());

    // Spawn the socket message loop
    plog!("Spawning socket message loop...");
    let loop_handle = tokio::spawn(loop_fut);
    plog!("Socket loop spawned");

    // Create our peer ID and room state
    let peer_id = uuid::Uuid::new_v4().to_string();
    plog!("Our peer ID: {}", peer_id);
    nih_log!("Our peer ID: {}", peer_id);
    let _ = evt_tx.send(NetEvent::PeerIdAssigned(format!("{}@{}", room_name, &peer_id[..8])));

    // Create yrs room state (same CRDT as web app)
    plog!("Creating YrsRoomState...");
    let mut room = YrsRoomState::new(peer_id.clone());
    plog!("YrsRoomState created");

    // Track connected peers
    let mut peers: Vec<matchbox_socket::PeerId> = Vec::new();
    let mut last_broadcast_sv = room.state_vector();

    plog!("Entering main loop, waiting for peers...");

    // Track previous state for diffing
    let mut prev_peer_sets: HashMap<String, Vec<u8>> = HashMap::new();
    let mut prev_voice_states: HashMap<String, (Option<i32>, Option<PitchClass>)> = HashMap::new();

    let mut loop_count: u64 = 0;
    let loop_start = std::time::Instant::now();

    loop {
        loop_count += 1;
        // Log heartbeat every ~5 seconds (5000ms / 16ms = ~312 iterations)
        if loop_count % 312 == 0 {
            plog!("Heartbeat: {} loops, {} peers, uptime {:?}",
                loop_count, peers.len(), loop_start.elapsed());
        }
        // Check for shutdown
        match cmd_rx.try_recv() {
            Ok(NetCommand::Shutdown) => {
                nih_log!("Networking thread shutting down");
                loop_handle.abort();
                return Ok(());
            }
            Ok(NetCommand::JoinChannel(new_channel)) => {
                // For now, log and ignore - would need to rebuild socket
                nih_log!("Channel switch requested to {} (not implemented yet)", new_channel);
            }
            Ok(NetCommand::SetPitchClass { pitch_class, on }) => {
                if on {
                    room.add_pitch(PitchClass(pitch_class));
                } else {
                    room.remove_pitch(PitchClass(pitch_class));
                }
            }
            Ok(NetCommand::SetVoicePitch(pitch)) => {
                room.set_voice_pitch(pitch);
                if let Some(p) = pitch {
                    // Also set pitch class (mod 12 for standard tuning)
                    room.set_voice_pitchclass(Some(PitchClass((p % 12) as u8)));
                } else {
                    room.set_voice_pitchclass(None);
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                loop_handle.abort();
                return Ok(());
            }
        }

        // Check for peer updates
        let peer_updates = socket.update_peers();
        if !peer_updates.is_empty() {
            plog!("Got {} peer update(s)", peer_updates.len());
        }
        for (peer_id, state) in peer_updates {
            match state {
                PeerState::Connected => {
                    plog!("*** PEER CONNECTED: {} ***", peer_id);
                    nih_log!("Peer connected: {}", peer_id);
                    peers.push(peer_id);
                    let _ = evt_tx.send(NetEvent::ConnectionStatus { connected_peers: peers.len() });

                    // Send our current state to the new peer
                    plog!("Sending state update to new peer...");
                    let state_update = room.encode_state_as_update();
                    plog!("State update size: {} bytes", state_update.len());
                    socket.channel_mut(0).send(state_update.into_boxed_slice(), peer_id);
                    plog!("State update sent");
                }
                PeerState::Disconnected => {
                    plog!("*** PEER DISCONNECTED: {} ***", peer_id);
                    nih_log!("Peer disconnected: {}", peer_id);
                    peers.retain(|p| *p != peer_id);
                    let _ = evt_tx.send(NetEvent::ConnectionStatus { connected_peers: peers.len() });

                    // Remove peer from room state
                    room.remove_peer(&peer_id.0.to_string());
                }
            }
        }

        // Handle incoming messages (yrs updates)
        let mut received_remote = false;
        let messages = socket.channel_mut(0).receive();
        if !messages.is_empty() {
            plog!("Received {} message(s)", messages.len());
        }
        for (peer_id, data) in messages {
            plog!("Received update from {} ({} bytes)", peer_id, data.len());
            nih_log!("Received update from {} ({} bytes)", peer_id, data.len());
            if let Err(e) = room.apply_update(&data) {
                plog!("Failed to apply update from {}: {}", peer_id, e);
                nih_warn!("Failed to apply update from {}: {}", peer_id, e);
            } else {
                received_remote = true;
                last_broadcast_sv = room.state_vector();
            }
        }

        // If we received remote updates, compute diffs and send events
        if received_remote {
            // Check for pitch class changes
            let peer_sets = room.all_peer_sets();
            for (remote_peer_id, peer_set) in &peer_sets {
                if remote_peer_id == &peer_id {
                    continue; // Skip our own changes
                }

                let current_pcs: Vec<u8> = peer_set.pitch_classes.iter().map(|pc| pc.0).collect();
                let prev_pcs = prev_peer_sets.get(remote_peer_id).cloned().unwrap_or_default();

                // Find added pitch classes
                for &pc in &current_pcs {
                    if !prev_pcs.contains(&pc) {
                        let _ = evt_tx.send(NetEvent::RemotePitchClassChange {
                            peer_id: remote_peer_id.clone(),
                            pitch_class: pc,
                            on: true,
                        });
                    }
                }

                // Find removed pitch classes
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

            // Check for voice state changes
            let voice_states = room.all_voice_states();
            for (remote_peer_id, (pitch, _pc)) in &voice_states {
                if remote_peer_id == &peer_id {
                    continue;
                }

                let prev = prev_voice_states.get(remote_peer_id).cloned();
                if prev.map(|(p, _)| p) != Some(*pitch) {
                    let _ = evt_tx.send(NetEvent::RemoteVoicePitchChange {
                        peer_id: remote_peer_id.clone(),
                        pitch: *pitch,
                    });
                }

                prev_voice_states.insert(remote_peer_id.clone(), (*pitch, *_pc));
            }
        }

        // Check for local changes and broadcast them
        if let Ok(update) = room.encode_diff(&last_broadcast_sv) {
            if !update.is_empty() && !peers.is_empty() {
                for peer in &peers {
                    socket.channel_mut(0).send(update.clone().into_boxed_slice(), *peer);
                }
                last_broadcast_sv = room.state_vector();
            }
        }

        // Yield to other tasks
        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }
}

// Plugin exports are in lib.rs at crate root
