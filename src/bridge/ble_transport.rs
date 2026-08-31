//! Signed Tutti BLE session pump over a platform [`BleHost`].
//!
//! The pump owns hello exchange, TOFU admission, the boot-bound ephemeral
//! handshake, fragmentation, authenticated lane framing, replay protection,
//! compatibility negotiation, and bounded repair ingress. The platform host
//! still owns OS permissions and GATT objects; HHHS admission remains outside
//! this module.

use std::collections::{BTreeSet, VecDeque};

use ed25519_dalek::SigningKey;
use tutti_ble::{
    CAPABILITY_HHHS_REPAIR, CAPABILITY_REALTIME, ControlFrame, DEFAULT_MAX_PAYLOAD_BYTES,
    HARD_MAX_WIRE_BYTES, Lane, LaneProfile, MIN_FRAGMENT_VALUE_BYTES, PeerHello, Reassembler,
    ReassemblyBudget, SessionCodec, channel_binding, decode_answer, decode_control_frame,
    encode_control_capability_bundle, encode_control_profile, realtime_generation,
    session_protocol_id, with_realtime_generation,
};
use tutti_realtime::{Frame as RealtimeFrame, MidiFrame, MidiKind};
use tutti_session::{EphemeralSecret, PeerIdentity, PendingInitiator};

use super::{
    BleHost, BleHostError, BleHostEvent, BleWriteMessage, BleWritePriority, BoardSessionBinding,
    BridgeCommand, BridgeError, BridgeTransport, LinkState, ProtocolProfile, RealtimeMidi,
    RealtimeMidiKind, TransportEvent,
};

const TRANSPORT_EVENT_CAPACITY: usize = 256;
const DEFAULT_REPAIR_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Debug)]
pub struct BleLinkConfig {
    pub identity_seed: [u8; 32],
    pub trusted_boards: BTreeSet<[u8; 32]>,
    pub local_profile: ProtocolProfile,
    pub max_payload_bytes: usize,
    pub max_repair_frame_bytes: usize,
    pub repair_queue_capacity: usize,
}

impl BleLinkConfig {
    pub fn walkie<I>(identity_seed: [u8; 32], trusted_boards: I) -> Self
    where
        I: IntoIterator<Item = [u8; 32]>,
    {
        Self {
            identity_seed,
            trusted_boards: trusted_boards.into_iter().collect(),
            local_profile: ProtocolProfile::WALKIE,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_repair_frame_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            repair_queue_capacity: DEFAULT_REPAIR_QUEUE_CAPACITY,
        }
    }

    fn validate(&self) -> Result<(), BridgeError> {
        if self.max_payload_bytes == 0 || self.max_payload_bytes > tutti_ble::HARD_MAX_PAYLOAD_BYTES
        {
            return Err(BridgeError::Transport(format!(
                "BLE payload budget must be within 1..={}",
                tutti_ble::HARD_MAX_PAYLOAD_BYTES
            )));
        }
        if self.max_repair_frame_bytes == 0
            || self.max_repair_frame_bytes > self.max_payload_bytes
            || self.max_repair_frame_bytes > HARD_MAX_WIRE_BYTES
        {
            return Err(BridgeError::Transport(
                "BLE repair-frame budget must fit the authenticated payload budget".into(),
            ));
        }
        if self.repair_queue_capacity == 0 {
            return Err(BridgeError::Transport(
                "BLE repair queue capacity is zero".into(),
            ));
        }
        Ok(())
    }
}

pub struct BleLinkTransport<H> {
    host: H,
    events: VecDeque<TransportEvent>,
    trusted: BTreeSet<[u8; 32]>,
    signing_key: SigningKey,
    local_hello: PeerHello,
    local_profile: ProtocolProfile,
    local_lane_profile: LaneProfile,
    remote_hello: Option<PeerHello>,
    trust_candidate: Option<[u8; 32]>,
    remote_lane_profile: Option<LaneProfile>,
    pending_provisioning: Option<(BoardSessionBinding, [u8; 32])>,
    pending: Option<PendingInitiator>,
    codec: Option<SessionCodec>,
    reassembler: Reassembler,
    repair_frames: VecDeque<Vec<u8>>,
    repair_queue_capacity: usize,
    negotiated_repair_frame_bytes: usize,
    next_message_id: u16,
    dropped_events: u64,
    desired_address: Option<super::BleAddress>,
    scan_requested: bool,
    connect_pending: bool,
    link_connected: bool,
}

impl<H> BleLinkTransport<H>
where
    H: BleHost,
{
    pub fn new(host: H, config: BleLinkConfig) -> Result<Self, BridgeError> {
        config.validate()?;
        let signing_key = SigningKey::from_bytes(&config.identity_seed);
        let local_hello = PeerHello {
            identity: PeerIdentity::from_signing_key(&signing_key),
            boot_nonce: rand::random(),
            // Replaced by the platform's negotiated ATT value size on connect.
            max_fragment_value_bytes: MIN_FRAGMENT_VALUE_BYTES as u16,
            capabilities: CAPABILITY_REALTIME | CAPABILITY_HHHS_REPAIR,
        };
        let local_lane_profile = lane_profile(
            config.local_profile,
            config.max_payload_bytes,
            config.max_repair_frame_bytes,
        )?;
        let local_wire_ceiling = tutti_ble::complete_wire_ceiling(config.max_payload_bytes)
            .filter(|ceiling| *ceiling <= HARD_MAX_WIRE_BYTES)
            .ok_or_else(|| BridgeError::Transport("BLE complete-wire ceiling is invalid".into()))?;
        Ok(Self {
            host,
            events: VecDeque::with_capacity(TRANSPORT_EVENT_CAPACITY),
            trusted: config.trusted_boards,
            signing_key,
            local_hello,
            local_profile: config.local_profile,
            local_lane_profile,
            remote_hello: None,
            trust_candidate: None,
            remote_lane_profile: None,
            pending_provisioning: None,
            pending: None,
            codec: None,
            reassembler: Reassembler::with_budget(
                ReassemblyBudget::new(local_wire_ceiling, 3, local_wire_ceiling * 3)
                    .map_err(wire_error)?,
            )
            .map_err(wire_error)?,
            repair_frames: VecDeque::with_capacity(config.repair_queue_capacity),
            repair_queue_capacity: config.repair_queue_capacity,
            negotiated_repair_frame_bytes: config.max_repair_frame_bytes,
            next_message_id: 0,
            dropped_events: 0,
            desired_address: None,
            scan_requested: false,
            connect_pending: false,
            link_connected: false,
        })
    }

    pub fn try_receive_repair_frame(&mut self) -> Option<Vec<u8>> {
        self.repair_frames.pop_front()
    }

    pub fn send_repair_frame(&mut self, frame: &[u8]) -> Result<(), BridgeError> {
        if !self
            .remote_hello
            .is_some_and(|hello| hello.supports(CAPABILITY_HHHS_REPAIR))
        {
            return Err(BridgeError::Unavailable(
                "BLE peer does not advertise an HHHS repair driver".into(),
            ));
        }
        if self.remote_lane_profile.is_none() {
            return Err(BridgeError::Unavailable(
                "BLE repair lane is not authenticated".into(),
            ));
        }
        if frame.len() > self.negotiated_repair_frame_bytes {
            return Err(BridgeError::Transport(format!(
                "BLE repair frame is {} bytes; negotiated limit is {}",
                frame.len(),
                self.negotiated_repair_frame_bytes
            )));
        }
        self.send_authenticated(Lane::HhhsRepair, frame)
    }

    pub fn send_round_table(&mut self, frame: tutti_roundtable::Frame) -> Result<(), BridgeError> {
        let encoded = tutti_realtime::encode(RealtimeFrame::RoundTable(frame))
            .map_err(|error| BridgeError::Transport(error.to_string()))?;
        self.send_authenticated(Lane::Realtime, encoded.as_bytes())
    }

    fn current_board_binding(&self) -> Option<BoardSessionBinding> {
        let hello = self.remote_hello?;
        let session_id = self.codec.as_ref()?.session_id();
        Some(BoardSessionBinding {
            identity: *hello.identity.as_bytes(),
            boot_nonce: hello.boot_nonce,
            session_id,
        })
    }

    fn send_capability_bundle(
        &mut self,
        binding: BoardSessionBinding,
        bundle: Vec<u8>,
    ) -> Result<(), BridgeError> {
        if self.current_board_binding() != Some(binding) {
            return Err(BridgeError::Transport(
                "capability bundle belongs to a stale board placement".into(),
            ));
        }
        if self.remote_lane_profile.is_none() {
            return Err(BridgeError::Unavailable(
                "board profile is not established for provisioning".into(),
            ));
        }
        let decoded = tutti_music_hhhs::decode_embedded_capability_bundle(&bundle)
            .map_err(|error| BridgeError::Transport(error.to_string()))?;
        if decoded.expected_receiver().as_bytes() != binding.identity {
            return Err(BridgeError::Transport(
                "capability bundle receiver differs from the authenticated board".into(),
            ));
        }
        let digest = *hhhs::Digest::of(&bundle).as_bytes();
        if let Some((pending_binding, pending_digest)) = self.pending_provisioning {
            return if pending_binding == binding && pending_digest == digest {
                Ok(())
            } else {
                Err(BridgeError::Transport(
                    "another capability bundle is already pending for this session".into(),
                ))
            };
        }
        let control = encode_control_capability_bundle(&bundle).map_err(wire_error)?;
        self.send_authenticated(Lane::Control, &control)?;
        self.pending_provisioning = Some((binding, digest));
        Ok(())
    }

    fn publish(&mut self, event: TransportEvent) {
        if self.events.len() == TRANSPORT_EVENT_CAPACITY {
            self.events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn reset_session(&mut self) {
        self.remote_hello = None;
        self.remote_lane_profile = None;
        self.pending_provisioning = None;
        self.pending = None;
        self.codec = None;
        self.reassembler.reset();
        self.repair_frames.clear();
        self.next_message_id = 0;
        self.negotiated_repair_frame_bytes =
            self.local_lane_profile.max_repair_frame_bytes as usize;
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        self.pending = None;
        self.codec = None;
        self.remote_lane_profile = None;
        self.pending_provisioning = None;
        self.reassembler.reset();
        self.scan_requested = false;
        self.connect_pending = false;
        self.link_connected = false;
        self.publish(TransportEvent::BoardLink(LinkState::Failed));
        self.publish(TransportEvent::Diagnostic(error.to_string()));
        // Operation, authentication, framing, and profile failures must not
        // spin in an automatic reconnect loop. A fresh explicit Connect starts
        // a new trust boundary after the diagnostic has been observed. A real
        // Disconnected host event has its own supervised reconnect path.
        self.desired_address = None;
        let _ = self.host.disconnect();
    }

    fn resume_desired_connection(&mut self) {
        if self.desired_address.is_some()
            && !self.scan_requested
            && !self.connect_pending
            && !self.link_connected
        {
            match self.host.start_scan() {
                Ok(()) => {
                    self.scan_requested = true;
                    self.publish(TransportEvent::BoardLink(LinkState::Discovering));
                }
                Err(error) => {
                    self.publish(TransportEvent::Diagnostic(ble_error(error).to_string()))
                }
            }
        }
    }

    fn begin_authentication(&mut self) -> Result<(), BridgeError> {
        if !self.link_connected {
            return Err(BridgeError::Transport(
                "cannot authenticate before the BLE connected event establishes fragmentation"
                    .into(),
            ));
        }
        let remote = self
            .remote_hello
            .ok_or_else(|| BridgeError::Transport("board hello has not been received".into()))?;
        if !remote.supports(CAPABILITY_REALTIME) {
            return Err(BridgeError::Transport(
                "board does not advertise the realtime lane".into(),
            ));
        }

        self.send_wire(&self.local_hello.encode(), BleWritePriority::Control)?;
        let (pending, offer) = PendingInitiator::begin(
            &self.signing_key,
            session_protocol_id(),
            channel_binding(self.local_hello, remote),
            rand::random(),
            EphemeralSecret::from_bytes(rand::random()),
        );
        self.pending = Some(pending);
        self.send_wire(&tutti_ble::encode_offer(&offer), BleWritePriority::Control)?;
        self.publish(TransportEvent::BoardLink(LinkState::Authenticating));
        Ok(())
    }

    fn send_wire(&mut self, wire: &[u8], priority: BleWritePriority) -> Result<(), BridgeError> {
        let remote = self
            .remote_hello
            .ok_or_else(|| BridgeError::Transport("cannot send before the board hello".into()))?;
        let value_bytes = usize::from(remote.max_fragment_value_bytes);
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1);
        let message = BleWriteMessage::new(message_id, wire.to_vec(), value_bytes, priority)
            .map_err(ble_error)?;
        self.host.write_rx_message(message).map_err(ble_error)
    }

    fn send_authenticated(&mut self, lane: Lane, payload: &[u8]) -> Result<(), BridgeError> {
        if self.remote_lane_profile.is_none() && lane != Lane::Control {
            return Err(BridgeError::Unavailable(
                "BLE lanes are not compatible and ready".into(),
            ));
        }
        let wire = self
            .codec
            .as_mut()
            .ok_or_else(|| BridgeError::Unavailable("BLE session is not authenticated".into()))?
            .encode(lane, payload)
            .map_err(wire_error)?;
        let priority = match lane {
            Lane::Control => BleWritePriority::Control,
            Lane::Realtime => BleWritePriority::Realtime,
            Lane::HhhsRepair => BleWritePriority::Repair,
        };
        self.send_wire(&wire, priority)
    }

    fn handle_host_event(&mut self, event: BleHostEvent) {
        let result = match event {
            BleHostEvent::ScanResult(result) => {
                let reconnect =
                    !self.connect_pending && self.desired_address.as_ref() == Some(&result.address);
                self.publish(TransportEvent::BoardDiscovered(result.clone()));
                if reconnect {
                    self.scan_requested = false;
                    match self.host.connect(&result.address).map_err(ble_error) {
                        Ok(()) => {
                            self.connect_pending = true;
                            self.publish(TransportEvent::BoardLink(LinkState::Connecting));
                        }
                        Err(error) => return self.fail(error),
                    }
                }
                Ok(())
            }
            BleHostEvent::Connected {
                max_fragment_value_bytes,
                ..
            } => {
                // Some platform stacks can surface a readable INFO value while
                // connect setup is still returning. Preserve that hello across
                // the new-link reset, but do not act on it until the negotiated
                // ATT value size below is installed.
                let early_hello = self.remote_hello;
                self.reset_session();
                self.remote_hello = early_hello;
                self.scan_requested = false;
                self.connect_pending = false;
                self.link_connected = true;
                if usize::from(max_fragment_value_bytes) < MIN_FRAGMENT_VALUE_BYTES {
                    Err(BridgeError::Transport(format!(
                        "negotiated GATT value is {max_fragment_value_bytes} bytes"
                    )))
                } else {
                    self.local_hello.max_fragment_value_bytes = max_fragment_value_bytes;
                    self.publish(TransportEvent::BoardLink(LinkState::Authenticating));
                    if let Some(hello) = early_hello {
                        self.continue_after_hello(hello)
                    } else {
                        Ok(())
                    }
                }
            }
            BleHostEvent::Disconnected { reason, .. } => {
                self.scan_requested = false;
                self.link_connected = false;
                self.reset_session();
                self.connect_pending = false;
                self.publish(TransportEvent::BoardLink(LinkState::Offline));
                if let Some(reason) = reason {
                    self.publish(TransportEvent::Diagnostic(reason));
                }
                self.resume_desired_connection();
                Ok(())
            }
            BleHostEvent::Info(bytes) => self.handle_info(&bytes),
            BleHostEvent::Notification(bytes) => self.handle_notification(&bytes),
            BleHostEvent::Diagnostic(message) => {
                self.publish(TransportEvent::Diagnostic(message));
                Ok(())
            }
            BleHostEvent::PermissionDenied => Err(ble_error(BleHostError::PermissionDenied)),
            BleHostEvent::EventsDropped(count) => Err(BridgeError::Transport(format!(
                "desktop BLE event queue dropped {count} values"
            ))),
            BleHostEvent::Error(error) => Err(ble_error(error)),
        };
        if let Err(error) = result {
            // OS-operation and framing failures end this explicit attempt.
            // Retrying them immediately can race a still-pending BlueZ call
            // and flood both the bounded command queue and the embedded GATT
            // callback. A real Disconnected event takes the supervised,
            // automatic reconnect path above; otherwise the user may retry
            // after seeing the concrete diagnostic.
            self.fail(error);
        }
    }

    fn handle_info(&mut self, bytes: &[u8]) -> Result<(), BridgeError> {
        let hello = PeerHello::decode(bytes).map_err(wire_error)?;
        if self.remote_hello == Some(hello) {
            return Ok(());
        }
        if self.remote_hello.is_some() || self.pending.is_some() || self.codec.is_some() {
            return Err(BridgeError::Transport(
                "board identity or boot hello changed during a connection".into(),
            ));
        }
        self.remote_hello = Some(hello);
        if !self.link_connected {
            // `Connected` will install the platform's fragmentation budget and
            // continue exactly once. Never start a minimum-MTU handshake merely
            // because INFO raced ahead in an OS callback queue.
            return Ok(());
        }
        self.continue_after_hello(hello)
    }

    fn continue_after_hello(&mut self, hello: PeerHello) -> Result<(), BridgeError> {
        if self.trusted.contains(hello.identity.as_bytes()) {
            self.trust_candidate = None;
            self.begin_authentication()
        } else {
            self.trust_candidate = Some(*hello.identity.as_bytes());
            self.publish(TransportEvent::TrustRequired(hello));
            Ok(())
        }
    }

    fn handle_notification(&mut self, bytes: &[u8]) -> Result<(), BridgeError> {
        if let Some(wire) = self.reassembler.push(bytes).map_err(wire_error)? {
            self.handle_wire(&wire)?;
        }
        Ok(())
    }

    fn handle_wire(&mut self, wire: &[u8]) -> Result<(), BridgeError> {
        if let Some(pending) = self.pending.take() {
            let remote = self.remote_hello.ok_or_else(|| {
                BridgeError::Transport("answer arrived before the board hello".into())
            })?;
            let answer = decode_answer(wire).map_err(wire_error)?;
            let keys = pending
                .complete(answer.as_bytes(), remote.identity)
                .map_err(|error| BridgeError::Transport(error.to_string()))?;
            self.codec = Some(
                SessionCodec::new(
                    keys,
                    self.local_lane_profile.max_authenticated_payload_bytes as usize,
                )
                .map_err(wire_error)?,
            );
            self.publish(TransportEvent::BoardLink(LinkState::Repairing));
            self.send_authenticated(
                Lane::Control,
                &encode_control_profile(self.local_lane_profile),
            )?;
            return Ok(());
        }

        let message = self
            .codec
            .as_mut()
            .ok_or_else(|| BridgeError::Transport("unexpected pre-session notification".into()))?
            .decode(wire)
            .map_err(wire_error)?;
        match message.lane {
            Lane::Control => self.handle_remote_control(&message.payload),
            Lane::Realtime => self.handle_realtime(&message.payload),
            Lane::HhhsRepair => self.handle_repair_frame(message.payload),
        }
    }

    fn handle_remote_control(&mut self, bytes: &[u8]) -> Result<(), BridgeError> {
        match decode_control_frame(bytes).map_err(wire_error)? {
            ControlFrame::Profile(profile) => self.handle_remote_profile(profile),
            ControlFrame::CapabilityBundle(_) => Err(BridgeError::Transport(
                "board sent a capability bundle to the provisioning host".into(),
            )),
            ControlFrame::RepairFin(_) | ControlFrame::RepairAck(_) => Err(
                BridgeError::Unavailable("BLE repair close driver is not attached".into()),
            ),
            ControlFrame::CapabilityReady(ready) => {
                let (binding, expected_digest) = self.pending_provisioning.ok_or_else(|| {
                    BridgeError::Transport(
                        "board acknowledged capability provisioning without a pending bundle"
                            .into(),
                    )
                })?;
                if self.current_board_binding() != Some(binding)
                    || ready.bundle_digest != expected_digest
                {
                    return Err(BridgeError::Transport(
                        "board capability acknowledgement is stale or names another bundle".into(),
                    ));
                }
                self.pending_provisioning = None;
                self.publish(TransportEvent::BoardProvisioned(binding));
                self.publish(TransportEvent::BoardLink(LinkState::Ready));
                Ok(())
            }
        }
    }

    fn handle_remote_profile(&mut self, profile: LaneProfile) -> Result<(), BridgeError> {
        if self.remote_lane_profile == Some(profile) {
            return Ok(());
        }
        if self.remote_lane_profile.is_some() {
            return Err(BridgeError::Transport(
                "board changed its protocol profile within one session".into(),
            ));
        }
        let remote = ProtocolProfile {
            music_generation: profile.music_generation,
            music_vocabulary_generation: profile.music_vocabulary_generation,
            hhhs_strategy_version: profile.hhhs_strategy_version,
            hhhs_repair_generation: profile.hhhs_repair_generation,
            room_generation: profile.application_generation,
            capabilities: profile.capabilities,
        };
        self.publish(TransportEvent::RemoteProfile(remote));
        if remote.capabilities & ProtocolProfile::CAP_REALTIME != 0 {
            let remote_generation = realtime_generation(profile.capabilities);
            if remote_generation != tutti_realtime::WIRE_GENERATION {
                self.publish(TransportEvent::BoardLink(LinkState::Refused));
                self.publish(TransportEvent::Diagnostic(format!(
                    "realtime wire generation differs (local {}, remote {})",
                    tutti_realtime::WIRE_GENERATION,
                    remote_generation
                )));
                return Ok(());
            }
        }
        if let Err(reason) = self.local_profile.check_compatible(remote) {
            self.publish(TransportEvent::BoardLink(LinkState::Refused));
            self.publish(TransportEvent::Diagnostic(reason.to_string()));
            return Ok(());
        }
        if profile.max_replica_record_bytes != self.local_lane_profile.max_replica_record_bytes {
            self.publish(TransportEvent::BoardLink(LinkState::Refused));
            self.publish(TransportEvent::Diagnostic(format!(
                "music record admission ceiling differs (local {}, remote {})",
                self.local_lane_profile.max_replica_record_bytes, profile.max_replica_record_bytes
            )));
            return Ok(());
        }
        let negotiated_payload = (profile.max_authenticated_payload_bytes as usize)
            .min(self.local_lane_profile.max_authenticated_payload_bytes as usize);
        self.codec
            .as_mut()
            .ok_or_else(|| BridgeError::Transport("profile arrived before authentication".into()))?
            .restrict_max_payload_bytes(negotiated_payload)
            .map_err(wire_error)?;
        let negotiated_wire = tutti_ble::complete_wire_ceiling(negotiated_payload)
            .ok_or_else(|| BridgeError::Transport("negotiated BLE wire ceiling overflow".into()))?;
        self.reassembler
            .restrict_budget(
                ReassemblyBudget::new(negotiated_wire, 3, negotiated_wire * 3)
                    .map_err(wire_error)?,
            )
            .map_err(wire_error)?;
        self.negotiated_repair_frame_bytes = (profile.max_repair_frame_bytes as usize)
            .min(self.local_lane_profile.max_repair_frame_bytes as usize)
            .min(negotiated_payload);
        self.remote_lane_profile = Some(profile);
        let binding = self.current_board_binding().ok_or_else(|| {
            BridgeError::Transport("profile established without an authenticated placement".into())
        })?;
        self.publish(TransportEvent::BoardProvisioningRequired(binding));
        Ok(())
    }

    fn handle_realtime(&mut self, bytes: &[u8]) -> Result<(), BridgeError> {
        if self.remote_lane_profile.is_none() {
            return Err(BridgeError::Transport(
                "realtime payload arrived before profile agreement".into(),
            ));
        }
        match tutti_realtime::decode(bytes)
            .map_err(|error| BridgeError::Transport(error.to_string()))?
        {
            RealtimeFrame::Midi(midi) => self.publish(TransportEvent::Midi(from_midi(midi))),
            RealtimeFrame::RoundTable(frame) => {
                self.publish(TransportEvent::RoundTable(frame));
            }
        }
        Ok(())
    }

    fn handle_repair_frame(&mut self, frame: Vec<u8>) -> Result<(), BridgeError> {
        if !self
            .remote_hello
            .is_some_and(|hello| hello.supports(CAPABILITY_HHHS_REPAIR))
        {
            return Err(BridgeError::Transport(
                "peer sent HHHS repair without advertising a repair driver".into(),
            ));
        }
        if self.remote_lane_profile.is_none() {
            return Err(BridgeError::Transport(
                "repair frame arrived before profile agreement".into(),
            ));
        }
        if frame.len() > self.negotiated_repair_frame_bytes {
            return Err(BridgeError::Transport(format!(
                "received BLE repair frame of {} bytes; negotiated limit is {}",
                frame.len(),
                self.negotiated_repair_frame_bytes
            )));
        }
        if self.repair_frames.len() == self.repair_queue_capacity {
            return Err(BridgeError::QueueFull {
                queue: "BLE repair ingress",
            });
        }
        self.repair_frames.push_back(frame);
        Ok(())
    }
}

impl<H> BridgeTransport for BleLinkTransport<H>
where
    H: BleHost,
{
    fn start(&mut self) -> Result<(), BridgeError> {
        self.publish(TransportEvent::TrustedBoards(
            u32::try_from(self.trusted.len()).unwrap_or(u32::MAX),
        ));
        Ok(())
    }

    fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
        match command {
            BridgeCommand::StartBoardScan => {
                if self.scan_requested || self.connect_pending || self.link_connected {
                    return Ok(());
                }
                self.host.start_scan().map_err(ble_error)?;
                self.scan_requested = true;
                self.publish(TransportEvent::BoardLink(LinkState::Discovering));
                Ok(())
            }
            BridgeCommand::ConnectBoard(address) => {
                if self.desired_address.as_ref() == Some(&address)
                    && (self.connect_pending || self.link_connected)
                {
                    return Ok(());
                }
                // Keep discovery alive until the platform has acquired the
                // peripheral. BlueZ can discard an unpaired, transient device
                // as soon as discovery stops, making the just-selected board
                // impossible to resolve by address. Platform hosts stop their
                // scan after a successful connection instead.
                self.desired_address = Some(address.clone());
                self.scan_requested = false;
                self.connect_pending = true;
                self.link_connected = false;
                self.trust_candidate = None;
                self.reset_session();
                if let Err(error) = self.host.connect(&address).map_err(ble_error) {
                    self.connect_pending = false;
                    return Err(error);
                }
                self.publish(TransportEvent::BoardLink(LinkState::Connecting));
                Ok(())
            }
            BridgeCommand::DisconnectBoard => {
                self.desired_address = None;
                self.scan_requested = false;
                self.connect_pending = false;
                self.link_connected = false;
                self.trust_candidate = None;
                self.host.disconnect().map_err(ble_error)?;
                Ok(())
            }
            BridgeCommand::TrustBoard(identity) => {
                // A UI command can be delivered twice (for example around a
                // repaint or disconnect callback). Once accepted, trusting the
                // same identity is idempotent and must not restart or poison an
                // in-flight authenticated session.
                if self.trusted.contains(&identity) {
                    let Some(hello) = self.remote_hello else {
                        return Ok(());
                    };
                    if hello.identity.as_bytes() != &identity {
                        return Err(BridgeError::Transport(
                            "trust decision does not match the connected board".into(),
                        ));
                    }
                    if self.pending.is_some() || self.codec.is_some() {
                        return Ok(());
                    }
                    return if self.link_connected {
                        self.begin_authentication()
                    } else {
                        Ok(())
                    };
                }
                let observed = self
                    .remote_hello
                    .map(|hello| *hello.identity.as_bytes())
                    .or(self.trust_candidate);
                if observed != Some(identity) {
                    return Err(BridgeError::Transport(
                        "trust decision does not match a board identity shown by this session"
                            .into(),
                    ));
                }
                self.trusted.insert(identity);
                self.trust_candidate = None;
                self.publish(TransportEvent::TrustedBoards(
                    u32::try_from(self.trusted.len()).unwrap_or(u32::MAX),
                ));
                if self.remote_hello.is_some() && self.link_connected {
                    self.begin_authentication()
                } else if self.remote_hello.is_none() {
                    // The exact prompted identity remains safe to trust after
                    // a transient link loss. Discovery is already supervising
                    // the selected address; its next hello continues here
                    // without another Connect or Trust click.
                    self.resume_desired_connection();
                    Ok(())
                } else {
                    // INFO raced ahead of Connected. The trust decision is
                    // retained and Connected will continue automatically.
                    Ok(())
                }
            }
            BridgeCommand::ForgetBoard(identity) => {
                self.trusted.remove(&identity);
                self.publish(TransportEvent::TrustedBoards(
                    u32::try_from(self.trusted.len()).unwrap_or(u32::MAX),
                ));
                Ok(())
            }
            BridgeCommand::SendBoardCapabilityBundle { binding, bundle } => {
                self.send_capability_bundle(binding, bundle)
            }
            BridgeCommand::SendBoardRoundTable(frame) => self.send_round_table(frame),
            BridgeCommand::ConfigureBle {
                identity_seed,
                trusted_boards,
            } => {
                if self.remote_hello.is_some() || self.pending.is_some() || self.codec.is_some() {
                    return Err(BridgeError::Transport(
                        "cannot replace BLE identity or trust while connected".into(),
                    ));
                }
                self.signing_key = SigningKey::from_bytes(&identity_seed);
                self.local_hello.identity = PeerIdentity::from_signing_key(&self.signing_key);
                self.local_hello.boot_nonce = rand::random();
                self.trusted = trusted_boards.into_iter().collect();
                self.trust_candidate = None;
                self.publish(TransportEvent::TrustedBoards(
                    u32::try_from(self.trusted.len()).unwrap_or(u32::MAX),
                ));
                Ok(())
            }
            BridgeCommand::ConfigureRoomIdentity { .. }
            | BridgeCommand::SelectRoom(_)
            | BridgeCommand::LeaveRoom
            | BridgeCommand::PrepareBoardProvisioning(_)
            | BridgeCommand::PublishRoundTable(_)
            | BridgeCommand::PublishBoardEdit { .. }
            | BridgeCommand::SetSharedPitch { .. } => Err(BridgeError::Unavailable(
                "native Iroh bridge is not enabled in this adapter".into(),
            )),
        }
    }

    fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
        let encoded = tutti_realtime::encode(RealtimeFrame::Midi(to_midi(event)?))
            .map_err(|error| BridgeError::Transport(error.to_string()))?;
        self.send_authenticated(Lane::Realtime, encoded.as_bytes())
    }

    fn poll_event(&mut self) -> Option<TransportEvent> {
        if self.dropped_events != 0 {
            let dropped = std::mem::take(&mut self.dropped_events);
            return Some(TransportEvent::Diagnostic(format!(
                "BLE transport event queue dropped {dropped} values"
            )));
        }
        for _ in 0..64 {
            if let Some(event) = self.events.pop_front() {
                return Some(event);
            }
            let event = self.host.poll_event()?;
            self.handle_host_event(event);
        }
        self.events.pop_front()
    }

    fn shutdown(&mut self) {
        self.desired_address = None;
        self.scan_requested = false;
        self.connect_pending = false;
        let _ = self.host.stop_scan();
        let _ = self.host.disconnect();
        self.link_connected = false;
        self.reset_session();
    }
}

fn lane_profile(
    profile: ProtocolProfile,
    max_payload_bytes: usize,
    max_repair_frame_bytes: usize,
) -> Result<LaneProfile, BridgeError> {
    let vocabulary = tutti_music_hhhs::MusicVocabularyProfile::embedded_compatible();
    Ok(LaneProfile {
        music_generation: profile.music_generation,
        music_vocabulary_generation: profile.music_vocabulary_generation,
        max_replica_record_bytes: u32::try_from(vocabulary.max_replica_record_bytes).map_err(
            |_| BridgeError::Transport("music record budget does not fit the profile wire".into()),
        )?,
        hhhs_strategy_version: profile.hhhs_strategy_version,
        hhhs_repair_generation: profile.hhhs_repair_generation,
        application_generation: profile.room_generation,
        capabilities: with_realtime_generation(
            profile.capabilities,
            tutti_realtime::WIRE_GENERATION,
        ),
        max_authenticated_payload_bytes: u32::try_from(max_payload_bytes).map_err(|_| {
            BridgeError::Transport("BLE payload budget does not fit the profile wire".into())
        })?,
        max_repair_frame_bytes: u32::try_from(max_repair_frame_bytes).map_err(|_| {
            BridgeError::Transport("BLE repair budget does not fit the profile wire".into())
        })?,
    })
}

fn to_midi(event: RealtimeMidi) -> Result<MidiFrame, BridgeError> {
    let kind = match event.kind {
        RealtimeMidiKind::NoteOn => MidiKind::NoteOn,
        RealtimeMidiKind::NoteOff => MidiKind::NoteOff,
        RealtimeMidiKind::Choke => MidiKind::Choke,
        RealtimeMidiKind::PolyPressure => MidiKind::PolyPressure,
        RealtimeMidiKind::PitchBend => MidiKind::PitchBend,
        RealtimeMidiKind::ChannelPressure => MidiKind::ChannelPressure,
    };
    MidiFrame::from_normalized(event.voice_id, event.channel, event.note, kind, event.value)
        .map_err(|error| BridgeError::Transport(error.to_string()))
}

fn from_midi(event: MidiFrame) -> RealtimeMidi {
    let kind = match event.kind {
        MidiKind::NoteOn => RealtimeMidiKind::NoteOn,
        MidiKind::NoteOff => RealtimeMidiKind::NoteOff,
        MidiKind::Choke => RealtimeMidiKind::Choke,
        MidiKind::PolyPressure => RealtimeMidiKind::PolyPressure,
        MidiKind::PitchBend => RealtimeMidiKind::PitchBend,
        MidiKind::ChannelPressure => RealtimeMidiKind::ChannelPressure,
    };
    RealtimeMidi {
        timing: 0,
        voice_id: event.voice_id,
        channel: event.channel,
        note: event.note,
        kind,
        value: event.normalized_value(),
    }
}

fn ble_error(error: BleHostError) -> BridgeError {
    BridgeError::Transport(error.to_string())
}

fn wire_error(error: tutti_ble::BleWireError) -> BridgeError {
    BridgeError::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::bridge::{BleAddress, BleScanResult, BridgeTransport, InMemoryBleHost};
    use hhhs_store::MemoryStorage;
    use tutti_ble::{
        CapabilityReady, Fragment, Fragmenter, Reassembler, encode_control_capability_ready,
    };

    const ADDRESS: &str = "simulated-board";

    fn test_capability_bundle(receiver: [u8; 32]) -> Vec<u8> {
        let owner_key = SigningKey::from_bytes(&[0x6d; 32]);
        let owner = tutti_music_hhhs::ActorId::from_signing_key(&owner_key);
        let receiver = tutti_music_hhhs::ActorId::from_bytes(receiver);
        let namespace = hhhs::Digest::of(b"walkie simulated BLE provisioning");
        let vocabulary = tutti_music_hhhs::MusicVocabularyProfile::embedded_compatible();
        let (replica, root) = tutti_music_hhhs::initialize_with_vocabulary(
            namespace,
            owner,
            MemoryStorage::new(),
            Some(vocabulary),
        )
        .unwrap();
        let leaf = tutti_music_hhhs::delegate(&replica, namespace, root, &owner_key, receiver)
            .unwrap()
            .entry;
        let bundle =
            tutti_music_hhhs::export_embedded_capability_bundle(&replica, receiver, [leaf])
                .unwrap();
        tutti_music_hhhs::encode_embedded_capability_bundle(&bundle).unwrap()
    }

    fn await_test_ready(transport: &mut BleLinkTransport<SimulatedBleHost>) -> bool {
        let mut ready = false;
        for _ in 0..256 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            match event {
                TransportEvent::BoardProvisioningRequired(binding) => transport
                    .handle_command(BridgeCommand::SendBoardCapabilityBundle {
                        binding,
                        bundle: test_capability_bundle(binding.identity),
                    })
                    .unwrap(),
                TransportEvent::BoardLink(LinkState::Ready) => ready = true,
                _ => {}
            }
        }
        ready
    }

    #[test]
    fn preauth_declared_wire_above_local_ceiling_is_refused_before_allocation() {
        let mut config = BleLinkConfig::walkie([0x31; 32], []);
        config.max_payload_bytes = 1_536;
        config.max_repair_frame_bytes = 1_536;
        let mut transport = BleLinkTransport::new(InMemoryBleHost::default(), config).unwrap();
        let ceiling = tutti_ble::complete_wire_ceiling(1_536).unwrap();
        assert_eq!(transport.reassembler.budget().max_wire_bytes(), ceiling);

        let mut packet = [0_u8; 20];
        let used = Fragment {
            message_id: 1,
            total_bytes: u16::try_from(ceiling + 1).unwrap(),
            offset: 0,
            start: true,
            end: false,
            payload: &[0x55],
        }
        .encode_into(&mut packet)
        .unwrap();
        let error = transport
            .handle_notification(&packet[..used])
            .expect_err("declared pre-auth message above the local wire ceiling must fail");
        assert!(error.to_string().contains(&format!("maximum is {ceiling}")));
        assert_eq!(transport.reassembler.retained_partial_bytes(), 0);
    }

    struct SimulatedGattPeer {
        key: SigningKey,
        hello: PeerHello,
        profile: LaneProfile,
        initiator_hello: Option<PeerHello>,
        reassembler: Reassembler,
        codec: Option<SessionCodec>,
        next_message_id: u16,
        duplicate_realtime: bool,
    }

    impl SimulatedGattPeer {
        fn new(profile: ProtocolProfile, duplicate_realtime: bool) -> Self {
            let key = SigningKey::from_bytes(&[9; 32]);
            Self {
                hello: PeerHello {
                    identity: PeerIdentity::from_signing_key(&key),
                    boot_nonce: 99,
                    max_fragment_value_bytes: 20,
                    capabilities: CAPABILITY_REALTIME | CAPABILITY_HHHS_REPAIR,
                },
                profile: lane_profile(profile, DEFAULT_MAX_PAYLOAD_BYTES, 1_024).unwrap(),
                key,
                initiator_hello: None,
                reassembler: Reassembler::with_budget(
                    ReassemblyBudget::new(
                        tutti_ble::complete_wire_ceiling(DEFAULT_MAX_PAYLOAD_BYTES).unwrap(),
                        3,
                        tutti_ble::complete_wire_ceiling(DEFAULT_MAX_PAYLOAD_BYTES).unwrap() * 3,
                    )
                    .unwrap(),
                )
                .unwrap(),
                codec: None,
                next_message_id: 0,
                duplicate_realtime,
            }
        }

        fn receive(&mut self, value: &[u8], events: &mut VecDeque<BleHostEvent>) {
            let Some(wire) = self.reassembler.push(value).unwrap() else {
                return;
            };
            if self.initiator_hello.is_none() {
                self.initiator_hello = Some(PeerHello::decode(&wire).unwrap());
                return;
            }
            if self.codec.is_none() {
                let initiator = self.initiator_hello.unwrap();
                let offer = tutti_ble::decode_offer(&wire).unwrap();
                let verified = offer
                    .verify(
                        session_protocol_id(),
                        channel_binding(initiator, self.hello),
                    )
                    .unwrap();
                assert_eq!(verified.identity(), initiator.identity);
                let (answer, keys) = verified
                    .respond(&self.key, EphemeralSecret::from_bytes([8; 32]))
                    .unwrap();
                self.codec = Some(SessionCodec::new(keys, DEFAULT_MAX_PAYLOAD_BYTES).unwrap());
                self.queue_wire(&tutti_ble::encode_answer(&answer), initiator, events);
                return;
            }

            let message = self.codec.as_mut().unwrap().decode(&wire).unwrap();
            match message.lane {
                Lane::Control => {
                    let response_payload = match decode_control_frame(&message.payload).unwrap() {
                        ControlFrame::Profile(_) => encode_control_profile(self.profile),
                        ControlFrame::CapabilityBundle(bundle) => {
                            encode_control_capability_ready(CapabilityReady {
                                bundle_digest: *hhhs::Digest::of(bundle).as_bytes(),
                            })
                        }
                        other => panic!("unexpected simulated control frame {other:?}"),
                    };
                    let response = self
                        .codec
                        .as_mut()
                        .unwrap()
                        .encode(Lane::Control, &response_payload)
                        .unwrap();
                    self.queue_wire(&response, self.initiator_hello.unwrap(), events);
                }
                Lane::Realtime => {
                    tutti_realtime::decode(&message.payload).unwrap();
                    let response = self
                        .codec
                        .as_mut()
                        .unwrap()
                        .encode(Lane::Realtime, &message.payload)
                        .unwrap();
                    self.queue_wire(&response, self.initiator_hello.unwrap(), events);
                    if self.duplicate_realtime {
                        self.queue_wire(&response, self.initiator_hello.unwrap(), events);
                    }
                }
                Lane::HhhsRepair => {
                    let response = self
                        .codec
                        .as_mut()
                        .unwrap()
                        .encode(Lane::HhhsRepair, &message.payload)
                        .unwrap();
                    self.queue_wire(&response, self.initiator_hello.unwrap(), events);
                }
            }
        }

        fn queue_wire(
            &mut self,
            wire: &[u8],
            initiator: PeerHello,
            events: &mut VecDeque<BleHostEvent>,
        ) {
            let message_id = self.next_message_id;
            self.next_message_id = self.next_message_id.wrapping_add(1);
            let value_bytes = usize::from(initiator.max_fragment_value_bytes);
            let mut value = vec![0; value_bytes];
            for fragment in Fragmenter::new(message_id, wire, value_bytes).unwrap() {
                let used = fragment.encode_into(&mut value).unwrap();
                events.push_back(BleHostEvent::Notification(value[..used].to_vec()));
            }
        }
    }

    struct SimulatedBleHost {
        events: VecDeque<BleHostEvent>,
        peer: SimulatedGattPeer,
        connected: bool,
        scan_starts: usize,
        scan_stops: usize,
        connects: usize,
    }

    impl SimulatedBleHost {
        fn new(profile: ProtocolProfile, duplicate_realtime: bool) -> Self {
            Self {
                events: VecDeque::new(),
                peer: SimulatedGattPeer::new(profile, duplicate_realtime),
                connected: false,
                scan_starts: 0,
                scan_stops: 0,
                connects: 0,
            }
        }
    }

    impl BleHost for SimulatedBleHost {
        fn start_scan(&mut self) -> Result<(), BleHostError> {
            self.scan_starts += 1;
            self.events
                .push_back(BleHostEvent::ScanResult(BleScanResult {
                    address: BleAddress(ADDRESS.into()),
                    display_name: Some("Simulated Tutti".into()),
                    signal_dbm: Some(-30),
                }));
            Ok(())
        }

        fn stop_scan(&mut self) -> Result<(), BleHostError> {
            self.scan_stops += 1;
            Ok(())
        }

        fn connect(&mut self, address: &BleAddress) -> Result<(), BleHostError> {
            assert_eq!(address.0, ADDRESS);
            self.connects += 1;
            self.connected = true;
            self.events.push_back(BleHostEvent::Connected {
                address: address.clone(),
                max_fragment_value_bytes: 20,
            });
            self.events
                .push_back(BleHostEvent::Info(self.peer.hello.encode().to_vec()));
            Ok(())
        }

        fn disconnect(&mut self) -> Result<(), BleHostError> {
            self.connected = false;
            Ok(())
        }

        fn write_rx_message(&mut self, message: BleWriteMessage) -> Result<(), BleHostError> {
            if !self.connected {
                return Err(BleHostError::Operation("not connected".into()));
            }
            let (_, mut cursor) = message.into_cursor()?;
            let mut value = vec![0; cursor.fragment_value_bytes()];
            while let Some(used) = cursor
                .encode_next(&mut value)
                .map_err(|error| BleHostError::Operation(error.to_string()))?
            {
                self.peer.receive(&value[..used], &mut self.events);
            }
            Ok(())
        }

        fn poll_event(&mut self) -> Option<BleHostEvent> {
            self.events.pop_front()
        }
    }

    #[test]
    fn connect_keeps_transient_board_visible_until_platform_acquires_it() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        transport
            .handle_command(BridgeCommand::ConnectBoard(BleAddress(ADDRESS.into())))
            .unwrap();

        assert!(transport.host.connected);
        assert_eq!(transport.host.scan_stops, 0);
    }

    #[test]
    fn repeated_scan_and_connect_commands_are_coalesced() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        assert_eq!(transport.host.scan_starts, 1);

        let address = BleAddress(ADDRESS.into());
        transport
            .handle_command(BridgeCommand::ConnectBoard(address.clone()))
            .unwrap();
        transport
            .handle_command(BridgeCommand::ConnectBoard(address))
            .unwrap();
        assert_eq!(transport.host.connects, 1);
    }

    #[test]
    fn idle_scan_error_does_not_start_an_immediate_retry_loop() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport.desired_address = Some(BleAddress(ADDRESS.into()));
        transport.scan_requested = true;

        transport.handle_host_event(BleHostEvent::Error(BleHostError::Operation(
            "start scan: adapter is busy".into(),
        )));

        assert_eq!(transport.host.scan_starts, 0);
        assert!(!transport.scan_requested);
        assert!(transport.desired_address.is_none());
    }

    #[test]
    fn connect_error_does_not_start_an_immediate_retry_loop() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport.desired_address = Some(BleAddress(ADDRESS.into()));
        transport.connect_pending = true;

        transport.handle_host_event(BleHostEvent::Error(BleHostError::Operation(
            "connect: timed out".into(),
        )));

        assert_eq!(transport.host.scan_starts, 0);
        assert!(!transport.connect_pending);
        assert!(transport.desired_address.is_none());
    }

    #[test]
    fn host_diagnostic_does_not_poison_an_installed_or_desired_connection() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        let address = BleAddress(ADDRESS.into());
        transport.desired_address = Some(address.clone());
        transport.link_connected = true;

        transport.handle_host_event(BleHostEvent::Diagnostic(
            "optional INFO read unavailable; awaiting notification".into(),
        ));

        assert!(transport.link_connected);
        assert_eq!(transport.desired_address, Some(address));
        assert!(transport.events.iter().any(|event| matches!(
            event,
            TransportEvent::Diagnostic(message) if message.contains("awaiting notification")
        )));
    }

    #[test]
    fn info_before_connected_waits_for_the_negotiated_fragmentation_budget() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport.start().unwrap();
        while transport.poll_event().is_some() {}

        transport.host.connected = true;
        let peer_hello = transport.host.peer.hello;
        transport.handle_host_event(BleHostEvent::Info(peer_hello.encode().to_vec()));
        assert!(!transport.link_connected);
        assert_eq!(
            transport.local_hello.max_fragment_value_bytes as usize,
            MIN_FRAGMENT_VALUE_BYTES
        );
        assert!(
            transport
                .events
                .iter()
                .all(|event| !matches!(event, TransportEvent::TrustRequired(_)))
        );

        transport.handle_host_event(BleHostEvent::Connected {
            address: BleAddress(ADDRESS.into()),
            max_fragment_value_bytes: 20,
        });
        let mut trust_required = false;
        while let Some(event) = transport.poll_event() {
            trust_required |= matches!(event, TransportEvent::TrustRequired(_));
        }
        assert!(trust_required);
        assert_eq!(transport.local_hello.max_fragment_value_bytes, 20);

        transport
            .handle_command(BridgeCommand::TrustBoard(*peer_hello.identity.as_bytes()))
            .unwrap();
        assert!(await_test_ready(&mut transport));
    }

    fn establish(duplicate_realtime: bool) -> BleLinkTransport<SimulatedBleHost> {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, duplicate_realtime);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport.start().unwrap();
        while transport.poll_event().is_some() {}
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        let mut discovered = false;
        while let Some(event) = transport.poll_event() {
            discovered |= matches!(event, TransportEvent::BoardDiscovered(_));
        }
        assert!(discovered);
        transport
            .handle_command(BridgeCommand::ConnectBoard(BleAddress(ADDRESS.into())))
            .unwrap();

        let peer_identity = *transport.host.peer.hello.identity.as_bytes();
        let mut trust_required = false;
        for _ in 0..32 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            trust_required |= matches!(event, TransportEvent::TrustRequired(_));
        }
        assert!(trust_required);
        transport
            .handle_command(BridgeCommand::TrustBoard(peer_identity))
            .unwrap();

        assert!(await_test_ready(&mut transport));
        transport
    }

    #[test]
    fn small_mtu_tofu_handshake_profile_and_realtime_echo_complete() {
        let mut transport = establish(false);
        let outbound = RealtimeMidi {
            timing: 27,
            voice_id: 4,
            channel: 2,
            note: 67,
            kind: RealtimeMidiKind::PitchBend,
            value: 0.75,
        };
        transport.send_realtime(outbound).unwrap();
        let mut received = None;
        for _ in 0..64 {
            if let Some(TransportEvent::Midi(event)) = transport.poll_event() {
                received = Some(event);
                break;
            }
        }
        let received = received.expect("simulated peer should echo MIDI");
        assert_eq!(received.timing, 0);
        assert_eq!(received.voice_id, outbound.voice_id);
        assert!((received.value - outbound.value).abs() < 0.000_1);
    }

    #[test]
    fn duplicate_authenticated_realtime_frame_fails_the_link() {
        let mut transport = establish(true);
        transport
            .send_realtime(RealtimeMidi {
                timing: 0,
                voice_id: RealtimeMidi::NO_VOICE_ID,
                channel: 0,
                note: 60,
                kind: RealtimeMidiKind::NoteOn,
                value: 1.0,
            })
            .unwrap();
        let mut saw_midi = false;
        let mut failed = false;
        for _ in 0..64 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            saw_midi |= matches!(event, TransportEvent::Midi(_));
            failed |= event == TransportEvent::BoardLink(LinkState::Failed);
        }
        assert!(saw_midi);
        assert!(failed);
    }

    #[test]
    fn repair_frame_survives_small_mtu_fragmentation_byte_exactly() {
        let mut transport = establish(false);
        let frame = (0_u16..300).flat_map(u16::to_be_bytes).collect::<Vec<_>>();
        transport.send_repair_frame(&frame).unwrap();
        let mut received = None;
        for _ in 0..64 {
            let _ = transport.poll_event();
            if let Some(candidate) = transport.try_receive_repair_frame() {
                received = Some(candidate);
                break;
            }
        }
        assert_eq!(received.as_deref(), Some(frame.as_slice()));

        let oversized = vec![0; 1_025];
        assert!(matches!(
            transport.send_repair_frame(&oversized),
            Err(BridgeError::Transport(_))
        ));
    }

    #[test]
    fn incompatible_profile_is_refused_after_valid_authentication() {
        let mut incompatible = ProtocolProfile::TUTTI_LEAF;
        incompatible.hhhs_strategy_version += 1;
        let host = SimulatedBleHost::new(incompatible, false);
        let peer = *host.peer.hello.identity.as_bytes();
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [peer])).unwrap();
        transport
            .handle_command(BridgeCommand::ConnectBoard(BleAddress(ADDRESS.into())))
            .unwrap();
        let mut refused = false;
        let mut ready = false;
        for _ in 0..160 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            refused |= event == TransportEvent::BoardLink(LinkState::Refused);
            ready |= event == TransportEvent::BoardLink(LinkState::Ready);
        }
        assert!(refused);
        assert!(!ready);
    }

    #[test]
    fn incompatible_realtime_wire_is_refused_before_ready() {
        let mut host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        host.peer.profile.capabilities =
            with_realtime_generation(host.peer.profile.capabilities, 2);
        let peer = *host.peer.hello.identity.as_bytes();
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [peer])).unwrap();
        transport
            .handle_command(BridgeCommand::ConnectBoard(BleAddress(ADDRESS.into())))
            .unwrap();
        let mut refused = false;
        let mut ready = false;
        let mut diagnostic = false;
        for _ in 0..160 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            refused |= event == TransportEvent::BoardLink(LinkState::Refused);
            ready |= event == TransportEvent::BoardLink(LinkState::Ready);
            diagnostic |= matches!(
                event,
                TransportEvent::Diagnostic(ref message)
                    if message == &format!(
                        "realtime wire generation differs (local {}, remote 2)",
                        tutti_realtime::WIRE_GENERATION,
                    )
            );
        }
        assert!(refused);
        assert!(diagnostic);
        assert!(!ready);
    }

    #[test]
    fn trusting_an_already_trusted_board_is_idempotent() {
        let mut transport = establish(false);
        let identity = *transport.host.peer.hello.identity.as_bytes();

        transport
            .handle_command(BridgeCommand::TrustBoard(identity))
            .unwrap();
        transport.reset_session();
        transport
            .handle_command(BridgeCommand::TrustBoard(identity))
            .unwrap();
    }

    #[test]
    fn trust_after_link_loss_is_retained_and_reconnects_without_another_click() {
        let host = SimulatedBleHost::new(ProtocolProfile::TUTTI_LEAF, false);
        let mut transport =
            BleLinkTransport::new(host, BleLinkConfig::walkie([1; 32], [])).unwrap();
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        while transport.poll_event().is_some() {}
        transport
            .handle_command(BridgeCommand::ConnectBoard(BleAddress(ADDRESS.into())))
            .unwrap();

        let identity = *transport.host.peer.hello.identity.as_bytes();
        let mut prompted = false;
        for _ in 0..32 {
            let Some(event) = transport.poll_event() else {
                break;
            };
            prompted |= matches!(event, TransportEvent::TrustRequired(_));
        }
        assert!(prompted);

        transport.handle_host_event(BleHostEvent::Disconnected {
            address: BleAddress(ADDRESS.into()),
            reason: Some("test link loss".into()),
        });
        transport
            .handle_command(BridgeCommand::TrustBoard(identity))
            .unwrap();

        assert!(
            await_test_ready(&mut transport),
            "selected board should reconnect, authenticate, and provision"
        );
        assert!(transport.trusted.contains(&identity));
    }

    #[cfg(feature = "desktop-ble")]
    #[test]
    #[ignore = "requires a powered physical Tutti GATT board"]
    fn physical_tutti_board_completes_tofu_and_authenticated_profile_negotiation() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        use crate::bridge::BtleplugHost;

        let host = BtleplugHost::spawn().expect("desktop BLE worker should start");
        let mut transport = BleLinkTransport::new(host, BleLinkConfig::walkie([0x7d; 32], []))
            .expect("physical probe configuration should be valid");
        transport.start().unwrap();
        transport
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();

        let mut address = None;
        let mut diagnostics = Vec::new();
        let mut trace = Vec::new();
        let scan_deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < scan_deadline && address.is_none() {
            while let Some(event) = transport.poll_event() {
                trace.push(format!("{event:?}"));
                match event {
                    // BtleplugHost has already filtered this result by the
                    // Tutti service UUID. The compact ESP advertisement omits
                    // its optional local name to fit the legacy 31-byte
                    // advertising budget, and BlueZ may therefore expose the
                    // MAC address as the display name after its cache is
                    // refreshed. Do not turn a cosmetic name into a second
                    // discovery contract.
                    TransportEvent::BoardDiscovered(board) => address = Some(board.address),
                    TransportEvent::Diagnostic(message) => diagnostics.push(message),
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        let address = address.unwrap_or_else(|| {
            panic!(
                "no physical Tutti GATT board was discovered: diagnostics={diagnostics:#?} trace={trace:#?}"
            )
        });
        transport
            .handle_command(BridgeCommand::ConnectBoard(address))
            .unwrap();

        let mut ready = false;
        let mut trusted = None;
        let connect_deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < connect_deadline && !ready {
            while let Some(event) = transport.poll_event() {
                trace.push(format!("{event:?}"));
                match event {
                    TransportEvent::TrustRequired(hello) => {
                        let identity = *hello.identity.as_bytes();
                        transport
                            .handle_command(BridgeCommand::TrustBoard(identity))
                            .unwrap();
                        trusted = Some(identity);
                    }
                    TransportEvent::BoardLink(LinkState::Ready) => ready = true,
                    TransportEvent::Diagnostic(message) => diagnostics.push(message),
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        let retained = trusted.is_some_and(|identity| transport.trusted.contains(&identity));
        transport
            .handle_command(BridgeCommand::DisconnectBoard)
            .unwrap();
        transport.shutdown();
        assert!(
            ready,
            "physical Tutti authentication failed: diagnostics={diagnostics:#?} trace={trace:#?}"
        );
        assert!(retained, "TOFU identity was not retained");
    }
}
