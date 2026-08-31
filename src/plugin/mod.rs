//! nice-plug CLAP/VST3 shell for the shared Tutti bridge.

mod editor;
mod params;

use std::sync::Arc;

use nice_plug::prelude::*;

use crate::bridge::{
    AudioBridgePort, BridgeConfig, BridgeRuntime, BridgeTransport, RealtimeMidi, RealtimeMidiKind,
};

use params::TuttiBridgeParams;

pub struct TuttiWalkieSongie {
    params: Arc<TuttiBridgeParams>,
    bridge: BridgeRuntime,
    audio: AudioBridgePort,
    feedback_guard: RoutedEchoGuard,
}

/// Fixed-size, audio-thread-only guard for hosts that strip or replace CLAP
/// note IDs when routing a plugin's MIDI output back to its input.
///
/// Stable membership voice IDs are the primary origin marker. The short-lived
/// fingerprint is a one-shot fallback covering the next few process blocks;
/// it never allocates, locks, sleeps, or touches networking.
struct RoutedEchoGuard {
    block: u64,
    note_on_until: [u64; 128],
    note_off_until: [u64; 128],
}

impl Default for RoutedEchoGuard {
    fn default() -> Self {
        Self {
            block: 1,
            note_on_until: [0; 128],
            note_off_until: [0; 128],
        }
    }
}

impl RoutedEchoGuard {
    const RETENTION_BLOCKS: u64 = 4;

    fn begin_block(&mut self) {
        self.block = self.block.wrapping_add(1).max(1);
    }

    fn record_output(&mut self, event: RealtimeMidi) {
        if !event.is_membership_projection() || event.channel != 0 {
            return;
        }
        let deadline = self.block.saturating_add(Self::RETENTION_BLOCKS);
        match event.kind {
            RealtimeMidiKind::NoteOn => self.note_on_until[usize::from(event.note)] = deadline,
            RealtimeMidiKind::NoteOff => self.note_off_until[usize::from(event.note)] = deadline,
            _ => {}
        }
    }

    fn should_suppress_input(&mut self, event: RealtimeMidi) -> bool {
        if event.is_membership_projection() {
            match event.kind {
                RealtimeMidiKind::NoteOn => self.note_on_until[usize::from(event.note)] = 0,
                RealtimeMidiKind::NoteOff => self.note_off_until[usize::from(event.note)] = 0,
                _ => {}
            }
            return true;
        }
        if event.channel != 0 {
            return false;
        }
        let deadline = match event.kind {
            RealtimeMidiKind::NoteOn => &mut self.note_on_until[usize::from(event.note)],
            RealtimeMidiKind::NoteOff => &mut self.note_off_until[usize::from(event.note)],
            _ => return false,
        };
        if *deadline >= self.block {
            *deadline = 0;
            true
        } else {
            *deadline = 0;
            false
        }
    }
}

impl Default for TuttiWalkieSongie {
    fn default() -> Self {
        let params = Arc::new(TuttiBridgeParams::default());
        let bridge = spawn_bridge(&params);
        let audio = bridge.audio_port();
        Self {
            params,
            bridge,
            audio,
            feedback_guard: RoutedEchoGuard::default(),
        }
    }
}

fn spawn_bridge(params: &TuttiBridgeParams) -> BridgeRuntime {
    #[cfg(all(feature = "desktop-ble", feature = "native-net"))]
    {
        use crate::bridge::{
            BleLinkConfig, BleLinkTransport, BtleplugHost, CarrierLeg, CarrierLegKind,
            CompositeTransport, NativeRoomConfig, NativeRoomTransport,
        };

        let identity_seed = params
            .bridge_identity_seed
            .lock()
            .map(|seed| *seed)
            .unwrap_or_else(|_| rand::random());
        let trusted = params
            .trusted_boards
            .lock()
            .map(|trusted| trusted.clone())
            .unwrap_or_default();
        let room = NativeRoomTransport::spawn(NativeRoomConfig::new(identity_seed))
            .map(CarrierLeg::available)
            .unwrap_or_else(|error| {
                let reason = format!("could not start native Iroh room transport: {error}");
                tracing::warn!(%reason, "native Iroh unavailable; continuing with desktop BLE");
                CarrierLeg::unavailable(CarrierLegKind::Room, reason)
            });
        let board = BtleplugHost::spawn()
            .map_err(|error| error.to_string())
            .and_then(|host| {
                BleLinkTransport::new(host, BleLinkConfig::walkie(identity_seed, trusted))
                    .map_err(|error| error.to_string())
            });
        let board = board.map(CarrierLeg::available).unwrap_or_else(|error| {
            let reason = format!("could not start desktop BLE transport: {error}");
            tracing::warn!(%reason, "desktop BLE unavailable; continuing with native Iroh");
            CarrierLeg::unavailable(CarrierLegKind::Board, reason)
        });
        runtime_with_transport(CompositeTransport::new(room, board))
    }
    #[cfg(all(feature = "native-net", not(feature = "desktop-ble")))]
    {
        use crate::bridge::{NativeRoomConfig, NativeRoomTransport};

        let identity_seed = params
            .bridge_identity_seed
            .lock()
            .map(|seed| *seed)
            .unwrap_or_else(|_| rand::random());
        let room = match NativeRoomTransport::spawn(NativeRoomConfig::new(identity_seed)) {
            Ok(room) => room,
            Err(error) => {
                return unavailable_bridge(format!(
                    "could not start native Iroh room transport: {error}"
                ));
            }
        };
        runtime_with_transport(room)
    }
    #[cfg(not(feature = "native-net"))]
    {
        match BridgeRuntime::spawn(BridgeConfig::default()) {
            Ok(runtime) => runtime,
            Err(error) => unavailable_bridge(format!("could not start bridge worker: {error}")),
        }
    }
}

fn runtime_with_transport<T: BridgeTransport>(transport: T) -> BridgeRuntime {
    match BridgeRuntime::spawn_with_transport(BridgeConfig::default(), transport) {
        Ok(runtime) => runtime,
        Err(error) => unavailable_bridge(format!("could not start bridge worker: {error}")),
    }
}

fn unavailable_bridge(message: String) -> BridgeRuntime {
    tracing::error!(%message, "Tutti bridge startup failed");
    BridgeRuntime::unavailable(BridgeConfig::default(), message)
}

impl Plugin for TuttiWalkieSongie {
    const NAME: &'static str = "Tutti Walkie Songie";
    const VENDOR: &'static str = "Polyphonotopes";
    const URL: &'static str = "https://github.com/polyphonotopes";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: None,
        ..AudioIOLayout::const_default()
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();
    type Editor = nice_plug_egui::EguiEditor<editor::TuttiEditorApp>;

    fn setup_logger() -> Option<bool> {
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::level_filters::LevelFilter::INFO)
            .with_target(true)
            .with_ansi(false)
            .finish();
        Some(tracing::subscriber::set_global_default(subscriber).is_ok())
    }

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
        editor::create(self.params.clone(), self.bridge.handle())
    }

    fn activate(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        _context: &mut impl ActivateContext<Self>,
    ) -> bool {
        let handle = self.bridge.handle();
        let mut ready = true;
        #[cfg(feature = "desktop-ble")]
        {
            let identity_seed = self
                .params
                .bridge_identity_seed
                .lock()
                .map(|seed| *seed)
                .unwrap_or_else(|_| rand::random());
            let trusted_boards = self
                .params
                .trusted_boards
                .lock()
                .map(|trusted| trusted.clone())
                .unwrap_or_default();
            ready &= handle
                .try_command(crate::bridge::BridgeCommand::ConfigureBle {
                    identity_seed,
                    trusted_boards,
                })
                .is_ok();
            ready &= handle
                .try_command(crate::bridge::BridgeCommand::StartBoardScan)
                .is_ok();
        }
        #[cfg(feature = "native-net")]
        {
            let identity_seed = self
                .params
                .bridge_identity_seed
                .lock()
                .map(|seed| *seed)
                .unwrap_or_else(|_| rand::random());
            ready &= handle
                .try_command(crate::bridge::BridgeCommand::ConfigureRoomIdentity { identity_seed })
                .is_ok();
            let room = self
                .params
                .room_name
                .lock()
                .map(|room| room.trim().to_owned())
                .unwrap_or_default();
            if !room.is_empty() {
                ready &= handle
                    .try_command(crate::bridge::BridgeCommand::SelectRoom(room))
                    .is_ok();
            }
        }
        ready
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.feedback_guard.begin_block();
        self.audio
            .set_input_mode(self.params.midi_input_policy.value().into());
        let receive = self.params.receive_midi.value();
        self.audio.set_output_enabled(receive);
        let share = self.params.share_midi.value();
        let thru = self.params.midi_thru.value();
        let mut output_timing_floor = 0;
        while let Some(event) = context.next_event() {
            let realtime = to_realtime(event);
            let feedback = realtime
                .map(|event| self.feedback_guard.should_suppress_input(event))
                .unwrap_or(false);
            if share
                && !feedback
                && let Some(realtime) = realtime
            {
                let _ = self.audio.try_send(realtime);
            }
            if thru && !feedback {
                output_timing_floor = event.timing();
                context.send_event(event);
            }
        }

        while let Some(realtime) = self.audio.try_recv() {
            // When output is disabled, consume any edge already queued before
            // the atomic mode change but forward only releases. The background
            // endpoint shadow then converges to empty. Re-enabling reconstructs
            // the current room set with fresh note-ons.
            if receive || realtime.is_release() {
                let realtime = with_timing_floor(realtime, output_timing_floor);
                self.feedback_guard.record_output(realtime);
                context.send_event(from_realtime(realtime));
                output_timing_floor = realtime.timing;
            }
        }

        ProcessStatus::Normal
    }
}

/// Host event lists must be monotonically ordered within each process block.
/// Room projection edges arrive asynchronously with timing zero, so emitting
/// one after a later MIDI-thru event would violate CLAP/VST3 ordering. Clamp
/// those background edges to the most recent emitted offset without allocating
/// or carrying a host-block timestamp across the realtime boundary.
fn with_timing_floor(mut event: RealtimeMidi, floor: u32) -> RealtimeMidi {
    event.timing = event.timing.max(floor);
    event
}

impl ClapPlugin for TuttiWalkieSongie {
    const CLAP_ID: &'static str = "xyz.wondering.tutti-walkie-songie";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("A realtime MIDI and durable HHHS bridge for Walkie Songie and Tutti boards");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Utility, ClapFeature::NoteEffect];
}

impl Vst3Plugin for TuttiWalkieSongie {
    const VST3_CLASS_ID: [u8; 16] = *b"TuttiWalkieSong!";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Tools, Vst3SubCategory::Fx];
}

fn to_realtime(event: NoteEvent<()>) -> Option<RealtimeMidi> {
    let message = match event {
        NoteEvent::NoteOn {
            timing,
            voice_id,
            channel,
            note,
            velocity,
        } => RealtimeMidi {
            timing,
            voice_id: voice_id.unwrap_or(RealtimeMidi::NO_VOICE_ID),
            channel,
            note,
            kind: RealtimeMidiKind::NoteOn,
            value: velocity,
        },
        NoteEvent::NoteOff {
            timing,
            voice_id,
            channel,
            note,
            velocity,
        } => RealtimeMidi {
            timing,
            voice_id: voice_id.unwrap_or(RealtimeMidi::NO_VOICE_ID),
            channel,
            note,
            kind: RealtimeMidiKind::NoteOff,
            value: velocity,
        },
        NoteEvent::Choke {
            timing,
            voice_id,
            channel,
            note,
        } => RealtimeMidi {
            timing,
            voice_id: voice_id.unwrap_or(RealtimeMidi::NO_VOICE_ID),
            channel,
            note,
            kind: RealtimeMidiKind::Choke,
            value: 0.0,
        },
        NoteEvent::PolyPressure {
            timing,
            voice_id,
            channel,
            note,
            pressure,
        } => RealtimeMidi {
            timing,
            voice_id: voice_id.unwrap_or(RealtimeMidi::NO_VOICE_ID),
            channel,
            note,
            kind: RealtimeMidiKind::PolyPressure,
            value: pressure,
        },
        NoteEvent::MidiPitchBend {
            timing,
            channel,
            value,
        } => RealtimeMidi {
            timing,
            voice_id: RealtimeMidi::NO_VOICE_ID,
            channel,
            note: 0,
            kind: RealtimeMidiKind::PitchBend,
            value,
        },
        NoteEvent::MidiChannelPressure {
            timing,
            channel,
            pressure,
        } => RealtimeMidi {
            timing,
            voice_id: RealtimeMidi::NO_VOICE_ID,
            channel,
            note: 0,
            kind: RealtimeMidiKind::ChannelPressure,
            value: pressure,
        },
        _ => return None,
    };
    Some(message)
}

fn from_realtime(event: RealtimeMidi) -> NoteEvent<()> {
    let voice_id = (event.voice_id != RealtimeMidi::NO_VOICE_ID).then_some(event.voice_id);
    match event.kind {
        RealtimeMidiKind::NoteOn => NoteEvent::NoteOn {
            timing: event.timing,
            voice_id,
            channel: event.channel,
            note: event.note,
            velocity: event.value,
        },
        RealtimeMidiKind::NoteOff => NoteEvent::NoteOff {
            timing: event.timing,
            voice_id,
            channel: event.channel,
            note: event.note,
            velocity: event.value,
        },
        RealtimeMidiKind::Choke => NoteEvent::Choke {
            timing: event.timing,
            voice_id,
            channel: event.channel,
            note: event.note,
        },
        RealtimeMidiKind::PolyPressure => NoteEvent::PolyPressure {
            timing: event.timing,
            voice_id,
            channel: event.channel,
            note: event.note,
            pressure: event.value,
        },
        RealtimeMidiKind::PitchBend => NoteEvent::MidiPitchBend {
            timing: event.timing,
            channel: event.channel,
            value: event.value,
        },
        RealtimeMidiKind::ChannelPressure => NoteEvent::MidiChannelPressure {
            timing: event.timing,
            channel: event.channel,
            pressure: event.value,
        },
    }
}

nice_plug::nice_export_clap!(TuttiWalkieSongie);
nice_plug::nice_export_vst3!(TuttiWalkieSongie);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_conversion_preserves_note_release_and_pitch_bend() {
        let events = [
            NoteEvent::NoteOff {
                timing: 19,
                voice_id: Some(4),
                channel: 2,
                note: 67,
                velocity: 0.25,
            },
            NoteEvent::MidiPitchBend {
                timing: 2,
                channel: 7,
                value: 0.75,
            },
        ];
        for original in events {
            assert_eq!(from_realtime(to_realtime(original).unwrap()), original);
        }
    }

    #[test]
    fn feedback_guard_rejects_tagged_and_one_untagged_routed_echo() {
        let projected = RealtimeMidi {
            timing: 0,
            voice_id: RealtimeMidi::membership_voice_id(48),
            channel: 0,
            note: 48,
            kind: RealtimeMidiKind::NoteOn,
            value: 0.8,
        };
        let mut guard = RoutedEchoGuard::default();
        guard.record_output(projected);
        guard.begin_block();

        assert!(guard.should_suppress_input(projected));
        guard.record_output(projected);
        guard.begin_block();
        let untagged = RealtimeMidi {
            voice_id: RealtimeMidi::NO_VOICE_ID,
            ..projected
        };
        assert!(guard.should_suppress_input(untagged));
        assert!(!guard.should_suppress_input(untagged));
    }

    #[test]
    fn asynchronous_projection_edges_never_move_host_time_backwards() {
        let projected = RealtimeMidi {
            timing: 0,
            voice_id: RealtimeMidi::membership_voice_id(60),
            channel: 0,
            note: 60,
            kind: RealtimeMidiKind::NoteOn,
            value: 0.8,
        };

        assert_eq!(with_timing_floor(projected, 960).timing, 960);
        assert_eq!(with_timing_floor(projected, 0).timing, 0);
    }
}
