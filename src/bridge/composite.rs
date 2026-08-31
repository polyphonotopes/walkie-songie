//! Composition of the independent native-room and local-board carrier legs.

use std::collections::VecDeque;

use super::{BridgeCommand, BridgeError, BridgeTransport, RealtimeMidi, TransportEvent};

const PENDING_EVENT_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum CarrierLegKind {
    Room,
    Board,
}

/// Keeps one carrier failure observable without making the other carrier a
/// compile-time or runtime prerequisite. An unavailable leg still participates
/// in composition by publishing its failed link state and a diagnostic.
pub struct CarrierLeg<T> {
    transport: Option<T>,
    unavailable_reason: Option<String>,
    pending: VecDeque<TransportEvent>,
}

impl<T> CarrierLeg<T> {
    pub fn available(transport: T) -> Self {
        Self {
            transport: Some(transport),
            unavailable_reason: None,
            pending: VecDeque::new(),
        }
    }

    pub fn unavailable(kind: CarrierLegKind, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let link = match kind {
            CarrierLegKind::Room => TransportEvent::RoomLink(super::LinkState::Failed),
            CarrierLegKind::Board => TransportEvent::BoardLink(super::LinkState::Failed),
        };
        Self {
            transport: None,
            unavailable_reason: Some(reason.clone()),
            pending: VecDeque::from([link, TransportEvent::Diagnostic(reason)]),
        }
    }
}

impl<T> BridgeTransport for CarrierLeg<T>
where
    T: BridgeTransport,
{
    fn start(&mut self) -> Result<(), BridgeError> {
        match self.transport.as_mut() {
            Some(transport) => transport.start(),
            None => Ok(()),
        }
    }

    fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
        self.transport.as_mut().map_or_else(
            || {
                Err(BridgeError::Unavailable(
                    self.unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "carrier leg is unavailable".into()),
                ))
            },
            |transport| transport.handle_command(command),
        )
    }

    fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
        match self.transport.as_mut() {
            Some(transport) => transport.send_realtime(event),
            // Realtime is explicitly ephemeral. The failed link/status event is
            // already observable; allocating an error for every performance
            // frame would turn an absent optional carrier into unbounded churn.
            None => Ok(()),
        }
    }

    fn poll_event(&mut self) -> Option<TransportEvent> {
        self.pending.pop_front().or_else(|| {
            self.transport
                .as_mut()
                .and_then(BridgeTransport::poll_event)
        })
    }

    fn shutdown(&mut self) {
        if let Some(transport) = self.transport.as_mut() {
            transport.shutdown();
        }
    }
}

/// One bridge transport whose room and board legs retain independent
/// connection lifecycles. Inbound realtime frames cross to the other leg once;
/// local audio events are offered to both legs without making either one a
/// prerequisite for the other.
pub struct CompositeTransport<R, B> {
    room: R,
    board: B,
    pending: VecDeque<TransportEvent>,
    poll_room_first: bool,
    room_ready: bool,
    board_binding: Option<super::BoardSessionBinding>,
    provisioning_requested: Option<super::BoardSessionBinding>,
}

impl<R, B> CompositeTransport<R, B>
where
    R: BridgeTransport,
    B: BridgeTransport,
{
    pub fn new(room: R, board: B) -> Self {
        Self {
            room,
            board,
            pending: VecDeque::with_capacity(PENDING_EVENT_CAPACITY),
            poll_room_first: true,
            room_ready: false,
            board_binding: None,
            provisioning_requested: None,
        }
    }

    fn request_board_provisioning(&mut self) {
        let Some(binding) = self.board_binding else {
            return;
        };
        if !self.room_ready || self.provisioning_requested == Some(binding) {
            return;
        }
        match self
            .room
            .handle_command(BridgeCommand::PrepareBoardProvisioning(binding))
        {
            Ok(()) => self.provisioning_requested = Some(binding),
            Err(error) => self.diagnostic("room provisioning", error),
        }
    }

    fn diagnostic(&mut self, leg: &'static str, error: BridgeError) {
        if matches!(error, BridgeError::Unavailable(_)) {
            return;
        }
        if self.pending.len() == PENDING_EVENT_CAPACITY {
            self.pending.pop_front();
        }
        self.pending
            .push_back(TransportEvent::Diagnostic(format!("{leg}: {error}")));
    }

    fn fence_board_repair(
        &mut self,
        binding: super::BoardSessionBinding,
        leg: &'static str,
        error: BridgeError,
    ) {
        if self.board_binding != Some(binding) {
            return;
        }
        let _ = self
            .room
            .handle_command(BridgeCommand::AbortBoardRepair(binding));
        let _ = self
            .board
            .handle_command(BridgeCommand::AbortBoardRepair(binding));
        self.diagnostic(leg, error);
    }

    fn start_failed(&mut self, leg: CarrierLegKind, error: BridgeError) {
        let link = match leg {
            CarrierLegKind::Room => TransportEvent::RoomLink(super::LinkState::Failed),
            CarrierLegKind::Board => TransportEvent::BoardLink(super::LinkState::Failed),
        };
        for event in [
            link,
            TransportEvent::Diagnostic(format!("{leg:?} startup: {error}")),
        ] {
            if self.pending.len() == PENDING_EVENT_CAPACITY {
                self.pending.pop_front();
            }
            self.pending.push_back(event);
        }
    }

    fn route_room_event(&mut self, event: TransportEvent) -> TransportEvent {
        match &event {
            TransportEvent::RoomLink(super::LinkState::Ready) => {
                self.room_ready = true;
                self.provisioning_requested = None;
                self.request_board_provisioning();
            }
            TransportEvent::RoomLink(_) => {
                self.room_ready = false;
                self.provisioning_requested = None;
            }
            TransportEvent::RoomSelected(_) => {
                self.provisioning_requested = None;
                self.request_board_provisioning();
            }
            TransportEvent::BoardCapabilityBundlePrepared { binding, bundle } => {
                if self.board_binding == Some(*binding)
                    && let Err(error) =
                        self.board
                            .handle_command(BridgeCommand::SendBoardCapabilityBundle {
                                binding: *binding,
                                bundle: bundle.clone(),
                            })
                {
                    self.provisioning_requested = None;
                    self.diagnostic("board provisioning", error);
                }
            }
            TransportEvent::BoardProvisioningFailed { binding, .. } => {
                if self.board_binding == Some(*binding) {
                    self.provisioning_requested = None;
                }
            }
            TransportEvent::BoardRepairOutbound { binding, frame } => {
                if self.board_binding == Some(*binding)
                    && let Err(error) =
                        self.board
                            .handle_command(BridgeCommand::SendBoardRepairFrame {
                                binding: *binding,
                                frame: frame.clone(),
                            })
                {
                    self.fence_board_repair(*binding, "board repair output", error);
                }
            }
            TransportEvent::BoardRepairTerminal(binding) => {
                if self.board_binding == Some(*binding)
                    && let Err(error) = self
                        .board
                        .handle_command(BridgeCommand::FinishBoardRepair(*binding))
                {
                    self.fence_board_repair(*binding, "board repair close", error);
                }
            }
            TransportEvent::BoardRepairFailed { binding, .. } => {
                if self.board_binding == Some(*binding)
                    && let Err(error) = self
                        .board
                        .handle_command(BridgeCommand::AbortBoardRepair(*binding))
                {
                    self.diagnostic("board repair failure fence", error);
                }
            }
            TransportEvent::BoardRepairSynchronized(binding) => {
                if self.board_binding == Some(*binding)
                    && let Err(error) = self
                        .board
                        .handle_command(BridgeCommand::CompleteBoardRepair(*binding))
                {
                    self.fence_board_repair(*binding, "board repair completion", error);
                }
            }
            _ => {}
        }
        if let TransportEvent::Midi(midi) = &event
            && let Err(error) = self.board.send_realtime(*midi)
        {
            self.diagnostic("board forwarding", error);
        }
        event
    }

    fn route_board_event(&mut self, event: TransportEvent) -> TransportEvent {
        match &event {
            TransportEvent::BoardProvisioningRequired(binding) => {
                self.board_binding = Some(*binding);
                self.provisioning_requested = None;
                self.request_board_provisioning();
            }
            TransportEvent::BoardProvisioned(binding) => {
                if self.board_binding != Some(*binding) {
                    self.diagnostic(
                        "board provisioning",
                        BridgeError::Transport(
                            "stale board provisioning completion was ignored".into(),
                        ),
                    );
                } else if let Err(error) = self
                    .room
                    .handle_command(BridgeCommand::StartBoardRepair(*binding))
                {
                    self.fence_board_repair(*binding, "board repair start", error);
                }
            }
            TransportEvent::BoardRepairInbound { binding, frame } => {
                if self.board_binding == Some(*binding)
                    && let Err(error) =
                        self.room
                            .handle_command(BridgeCommand::ObserveBoardRepairFrame {
                                binding: *binding,
                                frame: frame.clone(),
                            })
                {
                    self.fence_board_repair(*binding, "board repair input", error);
                }
            }
            TransportEvent::BoardRepairCarrierClosed(binding) => {
                if self.board_binding == Some(*binding)
                    && let Err(error) = self
                        .room
                        .handle_command(BridgeCommand::ConfirmBoardRepairClose(*binding))
                {
                    self.fence_board_repair(*binding, "board repair confirmation", error);
                }
            }
            TransportEvent::BoardLink(link)
                if !matches!(link, super::LinkState::Ready | super::LinkState::Repairing) =>
            {
                if let Some(binding) = self.board_binding
                    && let Err(error) = self
                        .room
                        .handle_command(BridgeCommand::AbortBoardRepair(binding))
                {
                    self.diagnostic("board repair abandonment", error);
                }
                self.board_binding = None;
                self.provisioning_requested = None;
            }
            TransportEvent::Midi(midi) => {
                if let Err(error) = self.room.send_realtime(*midi) {
                    self.diagnostic("room forwarding", error);
                }
            }
            TransportEvent::RoundTable(frame) => {
                // Configuration echoes must reach the bridge core before the
                // room. Only the core knows whether this is confirmation of a
                // room target, an older echo, or a genuine board-origin edit.
                return TransportEvent::BoardRoundTable(*frame);
            }
            _ => {}
        }
        event
    }
}

impl<R, B> BridgeTransport for CompositeTransport<R, B>
where
    R: BridgeTransport,
    B: BridgeTransport,
{
    fn start(&mut self) -> Result<(), BridgeError> {
        let room = self.room.start();
        let board = self.board.start();
        if let Err(error) = room {
            self.start_failed(CarrierLegKind::Room, error);
        }
        if let Err(error) = board {
            self.start_failed(CarrierLegKind::Board, error);
        }
        Ok(())
    }

    fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
        match command {
            BridgeCommand::ConfigureRoomIdentity { .. }
            | BridgeCommand::SelectRoom(_)
            | BridgeCommand::LeaveRoom
            | BridgeCommand::PrepareBoardProvisioning(_)
            | BridgeCommand::StartBoardRepair(_)
            | BridgeCommand::ObserveBoardRepairFrame { .. }
            | BridgeCommand::ConfirmBoardRepairClose(_)
            | BridgeCommand::AbortBoardRepair(_)
            | BridgeCommand::PublishRoundTable(_)
            | BridgeCommand::PublishBoardEdit { .. }
            | BridgeCommand::SetSharedPitch { .. } => self.room.handle_command(command),
            BridgeCommand::SendBoardRoundTable(frame) => self
                .board
                .handle_command(BridgeCommand::SendBoardRoundTable(frame)),
            BridgeCommand::SendBoardCapabilityBundle { binding, bundle } => self
                .board
                .handle_command(BridgeCommand::SendBoardCapabilityBundle { binding, bundle }),
            BridgeCommand::SendBoardRepairFrame { binding, frame } => self
                .board
                .handle_command(BridgeCommand::SendBoardRepairFrame { binding, frame }),
            BridgeCommand::FinishBoardRepair(binding) => self
                .board
                .handle_command(BridgeCommand::FinishBoardRepair(binding)),
            BridgeCommand::CompleteBoardRepair(binding) => self
                .board
                .handle_command(BridgeCommand::CompleteBoardRepair(binding)),
            command => self.board.handle_command(command),
        }
    }

    fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
        let room = self.room.send_realtime(event);
        let board = self.board.send_realtime(event);
        match (room, board) {
            (Err(left), Err(_right)) => {
                if matches!(left, BridgeError::Unavailable(_)) {
                    // Realtime is intentionally ephemeral. With neither leg
                    // ready there is nothing to queue or retry.
                    Ok(())
                } else {
                    Err(left)
                }
            }
            _ => Ok(()),
        }
    }

    fn poll_event(&mut self) -> Option<TransportEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        let event = if self.poll_room_first {
            self.room
                .poll_event()
                .map(|event| (true, event))
                .or_else(|| self.board.poll_event().map(|event| (false, event)))
        } else {
            self.board
                .poll_event()
                .map(|event| (false, event))
                .or_else(|| self.room.poll_event().map(|event| (true, event)))
        }?;
        self.poll_room_first = !self.poll_room_first;
        Some(if event.0 {
            self.route_room_event(event.1)
        } else {
            self.route_board_event(event.1)
        })
    }

    fn shutdown(&mut self) {
        self.room.shutdown();
        self.board.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    #[cfg(all(feature = "desktop-ble", feature = "native-net"))]
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::bridge::{BoardSessionBinding, LinkState, RealtimeMidiKind};

    #[derive(Default)]
    struct LegState {
        events: VecDeque<TransportEvent>,
        realtime: Vec<RealtimeMidi>,
        commands: Vec<BridgeCommand>,
        starts: usize,
        fail_start: bool,
    }

    #[derive(Clone, Default)]
    struct TestLeg(Arc<Mutex<LegState>>);

    impl BridgeTransport for TestLeg {
        fn start(&mut self) -> Result<(), BridgeError> {
            let mut state = self.0.lock().unwrap();
            state.starts += 1;
            if state.fail_start {
                Err(BridgeError::Transport("injected start failure".into()))
            } else {
                Ok(())
            }
        }

        fn handle_command(&mut self, command: BridgeCommand) -> Result<(), BridgeError> {
            self.0.lock().unwrap().commands.push(command);
            Ok(())
        }

        fn send_realtime(&mut self, event: RealtimeMidi) -> Result<(), BridgeError> {
            self.0.lock().unwrap().realtime.push(event);
            Ok(())
        }

        fn poll_event(&mut self) -> Option<TransportEvent> {
            self.0.lock().unwrap().events.pop_front()
        }

        fn shutdown(&mut self) {}
    }

    fn note() -> RealtimeMidi {
        RealtimeMidi {
            timing: 0,
            voice_id: 4,
            channel: 1,
            note: 64,
            kind: RealtimeMidiKind::NoteOn,
            value: 0.75,
        }
    }

    #[test]
    fn room_commands_and_board_commands_keep_independent_owners() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let mut composite = CompositeTransport::new(room.clone(), board.clone());
        composite
            .handle_command(BridgeCommand::ConfigureRoomIdentity {
                identity_seed: [7; 32],
            })
            .unwrap();
        composite
            .handle_command(BridgeCommand::SelectRoom("bright-river-song".into()))
            .unwrap();
        composite
            .handle_command(BridgeCommand::StartBoardScan)
            .unwrap();
        composite.handle_command(BridgeCommand::LeaveRoom).unwrap();
        assert!(matches!(
            room.0.lock().unwrap().commands.as_slice(),
            [
                BridgeCommand::ConfigureRoomIdentity { identity_seed },
                BridgeCommand::SelectRoom(_),
                BridgeCommand::LeaveRoom
            ] if *identity_seed == [7; 32]
        ));
        assert_eq!(
            board.0.lock().unwrap().commands,
            [BridgeCommand::StartBoardScan]
        );
    }

    #[test]
    fn one_leg_start_failure_does_not_prevent_the_other_leg_starting() {
        let room = TestLeg::default();
        room.0.lock().unwrap().fail_start = true;
        let board = TestLeg::default();
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardLink(LinkState::Ready));
        let mut composite = CompositeTransport::new(room.clone(), board.clone());

        composite.start().unwrap();

        assert_eq!(room.0.lock().unwrap().starts, 1);
        assert_eq!(board.0.lock().unwrap().starts, 1);
        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::RoomLink(LinkState::Failed))
        );
        assert!(matches!(
            composite.poll_event(),
            Some(TransportEvent::Diagnostic(message)) if message.contains("Room startup")
        ));
        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::BoardLink(LinkState::Ready))
        );
    }

    #[test]
    fn unavailable_leg_is_observable_and_does_not_hide_available_leg_events() {
        let room: CarrierLeg<TestLeg> =
            CarrierLeg::unavailable(CarrierLegKind::Room, "native Iroh could not initialize");
        let board = TestLeg::default();
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardLink(LinkState::Ready));
        let mut composite = CompositeTransport::new(room, board);
        composite.start().unwrap();

        let events = std::iter::from_fn(|| composite.poll_event()).collect::<Vec<_>>();
        assert!(events.contains(&TransportEvent::RoomLink(LinkState::Failed)));
        assert!(events.contains(&TransportEvent::BoardLink(LinkState::Ready)));
        assert!(events.iter().any(
            |event| matches!(event, TransportEvent::Diagnostic(message) if message.contains("native Iroh"))
        ));
    }

    #[test]
    fn room_leave_does_not_disconnect_or_hide_the_board_leg() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardLink(LinkState::Ready));
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::RoomLink(LinkState::Offline));

        let mut composite = CompositeTransport::new(room.clone(), board.clone());
        composite.handle_command(BridgeCommand::LeaveRoom).unwrap();

        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::RoomLink(LinkState::Offline))
        );
        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::BoardLink(LinkState::Ready))
        );
        assert!(board.0.lock().unwrap().commands.is_empty());
        assert_eq!(room.0.lock().unwrap().commands, [BridgeCommand::LeaveRoom]);
    }

    #[test]
    fn room_round_table_is_observed_without_bypassing_core_composition() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let config = tutti_music::roundtable::RoundTableConfig::default();
        let frame = tutti_roundtable::Frame::Config(tutti_roundtable::ConfigState { config });
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::RoundTable(frame));
        let mut composite = CompositeTransport::new(room, board.clone());
        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::RoundTable(frame))
        );

        assert!(board.0.lock().unwrap().commands.is_empty());
    }

    #[test]
    fn board_config_reaches_core_before_any_room_publication() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let config = tutti_music::roundtable::RoundTableConfig::default();
        let frame = tutti_roundtable::Frame::Config(tutti_roundtable::ConfigState { config });
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::RoundTable(frame));
        let mut composite = CompositeTransport::new(room.clone(), board);

        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::BoardRoundTable(frame))
        );
        assert!(
            room.0.lock().unwrap().commands.is_empty(),
            "a board echo must not enter the room before core classification"
        );
    }

    #[test]
    fn board_edit_crosses_the_room_boundary_as_one_command() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let config = tutti_music::roundtable::RoundTableConfig::default();
        let frame = tutti_roundtable::Frame::Config(tutti_roundtable::ConfigState { config });
        let pitch = tutti_music::TunedPeriodicPitch {
            tuning_id: tutti_music::Tuning::twelve_tet().id(),
            pitch: tutti_music::Tuning::twelve_tet()
                .periodic_pitch_for_midi(64)
                .expect("MIDI E exists in twelve-tone tuning"),
        };
        let command = BridgeCommand::PublishBoardEdit {
            token: 7,
            frame,
            settings: None,
            pitch_edits: vec![(pitch, true)],
        };
        let mut composite = CompositeTransport::new(room.clone(), board.clone());

        composite.handle_command(command.clone()).unwrap();

        assert_eq!(room.0.lock().unwrap().commands, [command]);
        assert!(board.0.lock().unwrap().commands.is_empty());
    }

    #[test]
    fn authenticated_board_provisioning_is_bound_across_both_legs() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let binding = BoardSessionBinding {
            identity: [0x41; 32],
            boot_nonce: 7,
            session_id: 9,
        };
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::RoomLink(LinkState::Ready));
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardProvisioningRequired(binding));
        let mut composite = CompositeTransport::new(room.clone(), board.clone());

        for _ in 0..2 {
            let _ = composite.poll_event();
        }
        assert_eq!(
            room.0.lock().unwrap().commands,
            [BridgeCommand::PrepareBoardProvisioning(binding)]
        );

        let bundle = vec![1, 2, 3];
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardCapabilityBundlePrepared {
                binding,
                bundle: bundle.clone(),
            });
        let _ = composite.poll_event();
        assert_eq!(
            board.0.lock().unwrap().commands,
            [BridgeCommand::SendBoardCapabilityBundle { binding, bundle }]
        );
    }

    #[test]
    fn stale_capability_bundle_never_crosses_to_a_new_board_placement() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let current = BoardSessionBinding {
            identity: [0x51; 32],
            boot_nonce: 11,
            session_id: 13,
        };
        let stale = BoardSessionBinding {
            session_id: 12,
            ..current
        };
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::RoomLink(LinkState::Ready));
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardProvisioningRequired(current));
        let mut composite = CompositeTransport::new(room.clone(), board.clone());
        for _ in 0..2 {
            let _ = composite.poll_event();
        }
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardCapabilityBundlePrepared {
                binding: stale,
                bundle: vec![9],
            });
        let _ = composite.poll_event();
        assert!(board.0.lock().unwrap().commands.is_empty());
    }

    #[test]
    fn failed_room_repair_abandons_the_exact_board_attempt() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let binding = BoardSessionBinding {
            identity: [0x61; 32],
            boot_nonce: 17,
            session_id: 19,
        };
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardProvisioningRequired(binding));
        let mut composite = CompositeTransport::new(room.clone(), board.clone());
        let _ = composite.poll_event();
        board.0.lock().unwrap().commands.clear();

        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardRepairFailed {
                binding,
                reason: "injected stepwise failure".into(),
            });

        assert!(matches!(
            composite.poll_event(),
            Some(TransportEvent::BoardRepairFailed { binding: failed, .. }) if failed == binding
        ));
        assert_eq!(
            board.0.lock().unwrap().commands,
            [BridgeCommand::AbortBoardRepair(binding)]
        );
    }

    #[test]
    fn same_placement_freshness_completion_crosses_to_the_board_leg() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        let binding = BoardSessionBinding {
            identity: [0x71; 32],
            boot_nonce: 23,
            session_id: 29,
        };
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardProvisioningRequired(binding));
        let mut composite = CompositeTransport::new(room.clone(), board.clone());
        let _ = composite.poll_event();
        board.0.lock().unwrap().commands.clear();
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardRepairSynchronized(binding));

        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::BoardRepairSynchronized(binding))
        );
        assert_eq!(
            board.0.lock().unwrap().commands,
            [BridgeCommand::CompleteBoardRepair(binding)]
        );
    }

    #[test]
    fn inbound_realtime_crosses_once_and_remains_visible_to_audio() {
        let room = TestLeg::default();
        let board = TestLeg::default();
        room.0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::Midi(note()));
        board
            .0
            .lock()
            .unwrap()
            .events
            .push_back(TransportEvent::BoardLink(LinkState::Ready));
        let mut composite = CompositeTransport::new(room, board.clone());
        assert_eq!(composite.poll_event(), Some(TransportEvent::Midi(note())));
        assert_eq!(board.0.lock().unwrap().realtime, [note()]);
        assert_eq!(
            composite.poll_event(),
            Some(TransportEvent::BoardLink(LinkState::Ready))
        );
    }

    #[cfg(all(feature = "desktop-ble", feature = "native-net"))]
    #[test]
    #[ignore = "requires a powered generation-matched Tutti board and live Iroh networking"]
    fn physical_board_stays_authenticated_across_native_room_join_and_leave() {
        use std::{
            io::{ErrorKind, Read, Write},
            net::{SocketAddr, TcpStream},
        };

        use crate::bridge::{
            BleLinkConfig, BleLinkTransport, BridgeConfig, BridgeEvent, BridgeRuntime,
            BtleplugHost, MidiInputMode, NativeRoomConfig, NativeRoomTransport,
        };

        fn board_get(path: &str) -> Result<Vec<u8>, String> {
            let address = SocketAddr::from(([192, 168, 71, 1], 80));
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
                .map_err(|error| format!("connect to board HTTP endpoint: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .map_err(|error| format!("set board HTTP read timeout: {error}"))?;
            let request = format!("GET {path} HTTP/1.0\r\nHost: 192.168.71.1\r\n\r\n");
            stream
                .write_all(request.as_bytes())
                .map_err(|error| format!("request board HTTP endpoint {path}: {error}"))?;
            let mut response = Vec::new();
            if let Err(error) = stream.read_to_end(&mut response)
                && !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
            {
                return Err(format!("read board HTTP endpoint {path}: {error}"));
            }
            let status_line_end = response
                .windows(2)
                .position(|window| window == b"\r\n")
                .ok_or_else(|| format!("board HTTP endpoint {path} has no status line"))?;
            if !response[..status_line_end]
                .windows(5)
                .any(|window| window == b" 200 ")
            {
                return Err(format!(
                    "board HTTP endpoint {path} did not return 200: {}",
                    String::from_utf8_lossy(&response[..status_line_end])
                ));
            }
            Ok(response)
        }

        fn board_arp_notes() -> Result<Vec<u8>, String> {
            let response = board_get("/api/status")?;
            let body = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| &response[index + 4..])
                .ok_or_else(|| "board status response has no HTTP body".to_owned())?;
            // ESP-IDF may use chunked framing and leave framing bytes after
            // the complete JSON value while keeping the socket alive. Start
            // at the object itself and decode exactly one value.
            let json_start = body
                .iter()
                .position(|byte| *byte == b'{')
                .ok_or_else(|| "board status body has no JSON object".to_owned())?;
            let status: serde_json::Value =
                serde_json::Deserializer::from_slice(&body[json_start..])
                    .into_iter()
                    .next()
                    .ok_or_else(|| "board status body is empty".to_owned())?
                    .map_err(|error| format!("decode board status: {error}"))?;
            status
                .get("arp_notes")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| "board status has no arp_notes array".to_owned())?
                .iter()
                .map(|note| {
                    note.as_u64()
                        .and_then(|note| u8::try_from(note).ok())
                        .ok_or_else(|| "board status contains an invalid arp note".to_owned())
                })
                .collect()
        }

        let identity_seed = [0x6a; 32];
        let room = NativeRoomTransport::spawn(NativeRoomConfig::new(identity_seed))
            .expect("native Iroh room transport should start");
        let host = BtleplugHost::spawn().expect("desktop BLE transport should start");
        let board = BleLinkTransport::new(host, BleLinkConfig::walkie(identity_seed, []))
            .expect("physical BLE link configuration should be valid");
        let mut runtime = BridgeRuntime::spawn_with_transport(
            BridgeConfig::default(),
            CompositeTransport::new(room, board),
        )
        .expect("composite bridge worker should start");
        let handle = runtime.handle();
        let audio = runtime.audio_port();

        let outcome = (|| -> Result<(), String> {
            handle
                .try_command(BridgeCommand::StartBoardScan)
                .map_err(|error| error.to_string())?;
            let scan_deadline = Instant::now() + Duration::from_secs(20);
            let mut board_address = None;
            let mut diagnostics = Vec::new();
            while Instant::now() < scan_deadline && board_address.is_none() {
                while let Some(event) = handle.try_event() {
                    match event {
                        // BtleplugHost has already filtered the OS results by
                        // the complete Tutti service UUID. The ESP deliberately
                        // omits its optional local name because a legacy
                        // advertisement cannot fit both that name and the
                        // 128-bit discovery UUID.
                        BridgeEvent::BoardDiscovered(board) => {
                            board_address = Some(board.address);
                        }
                        BridgeEvent::Diagnostic(message) => diagnostics.push(message),
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            let board_address = board_address.ok_or_else(|| {
                format!("physical Tutti board was not discovered: {diagnostics:#?}")
            })?;
            handle
                .try_command(BridgeCommand::ConnectBoard(board_address))
                .map_err(|error| error.to_string())?;

            let board_deadline = Instant::now() + Duration::from_secs(45);
            while Instant::now() < board_deadline && handle.status().board_link != LinkState::Ready
            {
                while let Some(event) = handle.try_event() {
                    match event {
                        BridgeEvent::TrustRequired(hello) => handle
                            .try_command(BridgeCommand::TrustBoard(*hello.identity.as_bytes()))
                            .map_err(|error| error.to_string())?,
                        BridgeEvent::Diagnostic(message) => diagnostics.push(message),
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            if handle.status().board_link != LinkState::Ready {
                return Err(format!(
                    "physical board did not reach authenticated Ready: status={:?} diagnostics={diagnostics:#?}",
                    handle.status()
                ));
            }

            let room_name = crate::words::generate_room_name();
            handle
                .try_command(BridgeCommand::SelectRoom(room_name.clone()))
                .map_err(|error| error.to_string())?;
            let room_deadline = Instant::now() + Duration::from_secs(45);
            while Instant::now() < room_deadline && handle.status().room_link != LinkState::Ready {
                while let Some(event) = handle.try_event() {
                    if let BridgeEvent::Diagnostic(message) = event {
                        diagnostics.push(message);
                    }
                }
                if handle.status().board_link != LinkState::Ready {
                    return Err(format!(
                        "board left Ready while joining room {room_name}: {:?}",
                        handle.status()
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            if handle.status().room_link != LinkState::Ready {
                return Err(format!(
                    "native room {room_name} did not reach Ready: status={:?} diagnostics={diagnostics:#?}",
                    handle.status()
                ));
            }

            // `Ready` is a carrier state, not proof that the room projection
            // reached the board. Exercise the actual set-editing path and
            // require both the canonical room event and the ESP's independent
            // HTTP materialization to agree on each level transition.
            let target_degree = note().note % 12;
            let target_board_note = 48 + target_degree;
            let wait_for_level = |expected: bool,
                                  handle: &crate::bridge::BridgeHandle,
                                  diagnostics: &mut Vec<String>|
             -> Result<(), String> {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut room_confirmed = false;
                let mut last_board = None;
                while Instant::now() < deadline {
                    while let Some(event) = handle.try_event() {
                        match event {
                            BridgeEvent::PitchSet(shared) => {
                                let present =
                                    shared.pitch_classes.iter().any(|pitch| {
                                        pitch.degree.index() == u16::from(target_degree)
                                    }) || shared.pitches.iter().any(|pitch| {
                                        pitch.pitch.degree().index() == u16::from(target_degree)
                                    });
                                room_confirmed |= present == expected;
                            }
                            BridgeEvent::Diagnostic(message) => diagnostics.push(message),
                            _ => {}
                        }
                    }
                    match board_arp_notes() {
                        Ok(notes) => {
                            last_board = Some(notes.clone());
                            let board_confirmed = notes.contains(&target_board_note) == expected;
                            if room_confirmed && board_confirmed {
                                return Ok(());
                            }
                        }
                        Err(error) => diagnostics.push(error),
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(format!(
                    "room/board pitch level did not converge to {expected}: board={last_board:?} status={:?} diagnostics={diagnostics:#?}",
                    handle.status()
                ))
            };

            // Prove the opposite projection direction before exercising the
            // plugin input. An explicit board-local web edit is a command and
            // must become canonical room state; periodic ConfigSnapshot frames
            // are passive observations and can never author this transition.
            board_get(&format!("/api/round?action=toggle&n={target_board_note}"))?;
            wait_for_level(true, &handle, &mut diagnostics)?;
            board_get(&format!("/api/round?action=toggle&n={target_board_note}"))?;
            wait_for_level(false, &handle, &mut diagnostics)?;

            audio.set_input_mode(MidiInputMode::ToggleSet);
            audio
                .try_send(note())
                .map_err(|_| "bounded set-edit ingress rejected first note-on".to_owned())?;
            wait_for_level(true, &handle, &mut diagnostics)?;
            let mut release = note();
            release.kind = RealtimeMidiKind::NoteOff;
            release.value = 0.0;
            audio
                .try_send(release)
                .map_err(|_| "bounded set-edit ingress rejected first note-off".to_owned())?;
            audio
                .try_send(note())
                .map_err(|_| "bounded set-edit ingress rejected second note-on".to_owned())?;
            wait_for_level(false, &handle, &mut diagnostics)?;
            audio
                .try_send(release)
                .map_err(|_| "bounded set-edit ingress rejected second note-off".to_owned())?;

            audio.set_input_mode(MidiInputMode::Perform);
            audio
                .try_send(note())
                .map_err(|_| "bounded realtime ingress rejected note-on".to_owned())?;
            audio
                .try_send(release)
                .map_err(|_| "bounded realtime ingress rejected note-off".to_owned())?;

            handle
                .try_command(BridgeCommand::LeaveRoom)
                .map_err(|error| error.to_string())?;
            let leave_deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < leave_deadline && handle.status().room_link != LinkState::Offline
            {
                if handle.status().board_link != LinkState::Ready {
                    return Err(format!(
                        "board left Ready during room leave: {:?}",
                        handle.status()
                    ));
                }
                while let Some(event) = handle.try_event() {
                    if let BridgeEvent::Diagnostic(message) = event {
                        diagnostics.push(message);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            if handle.status().room_link != LinkState::Offline {
                return Err(format!(
                    "native room did not become Offline after leave: {:?}",
                    handle.status()
                ));
            }

            // Soak beyond several BLE polling intervals. A stale Ready value is
            // insufficient if the OS has already delivered link loss.
            let soak_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < soak_deadline {
                while let Some(event) = handle.try_event() {
                    if let BridgeEvent::Diagnostic(message) = event {
                        diagnostics.push(message);
                    }
                }
                if handle.status().board_link != LinkState::Ready {
                    return Err(format!(
                        "board did not remain Ready after room leave: status={:?} diagnostics={diagnostics:#?}",
                        handle.status()
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }

            let before_disconnect = board_arp_notes()?;
            if before_disconnect.contains(&target_board_note) {
                return Err(format!(
                    "removed pitch returned after room leave: board={before_disconnect:?} diagnostics={diagnostics:#?}"
                ));
            }

            handle
                .try_command(BridgeCommand::DisconnectBoard)
                .map_err(|error| error.to_string())?;
            let disconnect_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < disconnect_deadline
                && handle.status().board_link != LinkState::Offline
            {
                while let Some(event) = handle.try_event() {
                    if let BridgeEvent::Diagnostic(message) = event {
                        diagnostics.push(message);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            thread::sleep(Duration::from_secs(1));
            let after_disconnect = board_arp_notes()?;
            if after_disconnect.contains(&target_board_note) {
                return Err(format!(
                    "removed pitch returned after board disconnect: board={after_disconnect:?} diagnostics={diagnostics:#?}"
                ));
            }
            Ok(())
        })();

        let _ = handle.try_command(BridgeCommand::DisconnectBoard);
        runtime.shutdown();
        if let Err(error) = outcome {
            panic!("simultaneous native-room/physical-board acceptance failed: {error}");
        }
    }
}
