use std::{collections::VecDeque, sync::Arc, time::Duration};

use nice_plug::context::gui::GuiContext;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug::prelude::*;
use nice_plug_egui::{
    EguiEditor, EguiEditorState, EguiNiceSettings, NiceEguiApp, RepaintNotifier,
    create_egui_editor, widgets::ParamSlider,
};
use tutti_ble::PeerHello;

use crate::bridge::{BleScanResult, BridgeCommand, BridgeEvent, BridgeHandle, LinkState};

use super::params::TuttiBridgeParams;

const EDITOR_WIDTH: f32 = 640.0;
const EDITOR_HEIGHT: f32 = 620.0;
const PANEL: egui::Color32 = egui::Color32::from_rgb(23, 27, 38);
const PANEL_STROKE: egui::Color32 = egui::Color32::from_rgb(51, 59, 78);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(105, 221, 190);
const MUTED: egui::Color32 = egui::Color32::from_rgb(158, 169, 190);

pub fn create(
    params: Arc<TuttiBridgeParams>,
    bridge: BridgeHandle,
) -> Option<EguiEditor<TuttiEditorApp>> {
    let egui_state = EguiEditorState::from_size(LogicalSize::new(EDITOR_WIDTH, EDITOR_HEIGHT), 1.0);
    create_egui_editor(
        egui_state,
        RepaintNotifier::new(),
        EguiNiceSettings::new().with_tile("Tutti Walkie Songie"),
        TuttiEditorApp {
            state: EditorState::new(&params),
            params,
            bridge,
            gui_context: None,
        },
    )
}

pub struct TuttiEditorApp {
    state: EditorState,
    params: Arc<TuttiBridgeParams>,
    bridge: BridgeHandle,
    gui_context: Option<GuiContext>,
}

impl NiceEguiApp for TuttiEditorApp {
    fn build(
        &mut self,
        context: egui::Context,
        gui_context: GuiContext,
        _frame: &mut nice_plug_egui::Frame,
    ) -> Result<(), nice_plug_egui::baseview::HandlerError> {
        configure_style(&context);
        self.gui_context = Some(gui_context);
        Ok(())
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut nice_plug_egui::Frame) {
        let Some(gui_context) = self.gui_context.as_ref() else {
            ui.label("Plugin UI context is not ready");
            return;
        };
        let setter = gui_context.param_setter();
        draw_editor(ui, &setter, &mut self.state, &self.params, &self.bridge);
    }

    fn editor_closed(&mut self) {
        self.gui_context = None;
    }
}

struct EditorState {
    room: String,
    pending_trust: Option<PeerHello>,
    boards: Vec<BleScanResult>,
    diagnostics: VecDeque<String>,
}

impl EditorState {
    fn new(params: &TuttiBridgeParams) -> Self {
        Self {
            room: params
                .room_name
                .lock()
                .map(|room| room.clone())
                .unwrap_or_default(),
            pending_trust: None,
            boards: Vec::new(),
            diagnostics: VecDeque::new(),
        }
    }

    fn diagnostic(&mut self, message: impl Into<String>) {
        if self.diagnostics.len() == 8 {
            self.diagnostics.pop_front();
        }
        let message = message.into();
        if self.diagnostics.back() != Some(&message) {
            self.diagnostics.push_back(message);
        }
    }
}

fn configure_style(context: &egui::Context) {
    context.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(9.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
    });

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(14, 17, 24);
    visuals.window_fill = visuals.panel_fill;
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(45, 116, 101);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(42, 72, 78);
    visuals.selection.bg_fill = egui::Color32::from_rgb(42, 110, 96);
    context.set_visuals(visuals);
}

fn draw_editor(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    state: &mut EditorState,
    params: &TuttiBridgeParams,
    bridge: &BridgeHandle,
) {
    ui.ctx().request_repaint_after(Duration::from_millis(100));
    receive_events(state, bridge);
    let status = bridge.status();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.heading("Tutti / Walkie Songie");
            ui.label(
                egui::RichText::new("Iroh room + authenticated BLE board bridge").color(MUTED),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            status_pill(ui, "BOARD", status.board_link);
            status_pill(ui, "ROOM", status.room_link);
        });
    });
    ui.add_space(10.0);

    card(ui, |ui| {
        section_title(ui, "Iroh room", "Share live MIDI and durable HHHS state");
        ui.horizontal(|ui| {
            let room_width = (ui.available_width() - 190.0).max(160.0);
            let field = egui::TextEdit::singleline(&mut state.room)
                .hint_text("quiet-river-song")
                .desired_width(room_width);
            let response = ui.add(field);
            let submit =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            let can_join = !state.room.trim().is_empty()
                && !matches!(
                    status.room_link,
                    LinkState::Connecting | LinkState::Repairing
                );
            if ui
                .add_enabled(can_join, primary_button("Join room"))
                .clicked()
                || (submit && can_join)
            {
                join_room(state, params, bridge);
            }
            if ui
                .add_enabled(
                    status.room_link != LinkState::Offline,
                    egui::Button::new("Leave"),
                )
                .clicked()
            {
                if let Ok(mut saved) = params.room_name.lock() {
                    saved.clear();
                }
                command(state, bridge, BridgeCommand::LeaveRoom);
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(link_label(status.room_link))
                    .color(link_color(status.room_link)),
            );
            ui.label(egui::RichText::new(format!("{} peer(s)", status.room_peers)).color(MUTED));
            ui.label(
                egui::RichText::new("Saved rooms rejoin automatically")
                    .small()
                    .color(MUTED),
            );
        });
    });

    ui.add_space(9.0);
    card(ui, |ui| {
        section_title(ui, "Tutti board", "One Connect click; trust once per board");
        ui.horizontal(|ui| {
            if ui.button("Find boards").clicked() {
                command(state, bridge, BridgeCommand::StartBoardScan);
            }
            if ui
                .add_enabled(
                    status.board_link != LinkState::Offline,
                    egui::Button::new("Disconnect"),
                )
                .clicked()
            {
                state.pending_trust = None;
                command(state, bridge, BridgeCommand::DisconnectBoard);
            }
            ui.label(
                egui::RichText::new(format!(
                    "{}; {} trusted",
                    link_label(status.board_link),
                    status.trusted_boards
                ))
                .color(link_color(status.board_link)),
            );
        });

        let mut connect = None;
        if state.boards.is_empty() {
            ui.label(egui::RichText::new("Scanning for Tutti boards...").color(MUTED));
        } else {
            for board in &state.boards {
                ui.horizontal(|ui| {
                    let name = board
                        .display_name
                        .as_deref()
                        .unwrap_or(board.address.0.as_str());
                    ui.label(egui::RichText::new(name).strong());
                    if let Some(signal) = board.signal_dbm {
                        ui.label(egui::RichText::new(format!("{signal} dBm")).color(MUTED));
                    }
                    if ui
                        .add_enabled(
                            !matches!(
                                status.board_link,
                                LinkState::Connecting | LinkState::Authenticating
                            ),
                            primary_button("Connect"),
                        )
                        .clicked()
                    {
                        connect = Some(board.address.clone());
                    }
                });
            }
        }
        if let Some(address) = connect {
            command(state, bridge, BridgeCommand::ConnectBoard(address));
        }
    });

    if let Some(hello) = state.pending_trust {
        ui.add_space(9.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(55, 45, 22))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 142, 61)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Trust this Tutti board?").strong());
                ui.label(
                    egui::RichText::new(format!("Board identity {}", short_identity(hello)))
                        .monospace(),
                );
                ui.label(
                    egui::RichText::new(
                        "Trust is remembered. If the radio drops now, connection continues automatically.",
                    )
                    .small()
                    .color(egui::Color32::from_rgb(220, 206, 171)),
                );
                ui.horizontal(|ui| {
                    if ui.add(primary_button("Trust and continue")).clicked() {
                        trust_board(state, params, bridge, hello);
                    }
                    if ui.button("Cancel").clicked() {
                        state.pending_trust = None;
                        command(state, bridge, BridgeCommand::DisconnectBoard);
                    }
                });
            });
    }

    ui.add_space(9.0);
    card(ui, |ui| {
        section_title(ui, "MIDI routing", "Audio-thread-safe bounded queues");
        ui.horizontal(|ui| {
            ui.label("Input mode");
            ui.add(ParamSlider::for_param(&params.midi_input_policy, setter).with_width(160.0));
        });
        ui.label(
            egui::RichText::new(
                "Toggle edits membership on each strike; Gate removes on release; Perform sends transient notes.",
            )
            .small()
            .color(MUTED),
        );
        ui.horizontal_wrapped(|ui| {
            param_checkbox(ui, setter, &params.midi_thru);
            param_checkbox(ui, setter, &params.share_midi);
            param_checkbox(ui, setter, &params.receive_midi);
        });
        let dropped = status.realtime_ingress_dropped + status.realtime_egress_dropped;
        let color = if dropped == 0 {
            MUTED
        } else {
            egui::Color32::YELLOW
        };
        ui.label(
            egui::RichText::new(format!(
                "MIDI activity: {} from host / {} from room or board",
                status.realtime_ingress_events, status.realtime_egress_events
            ))
            .small()
            .color(if status.realtime_ingress_events == 0 {
                egui::Color32::from_rgb(247, 198, 96)
            } else {
                ACCENT
            }),
        );
        ui.label(
            egui::RichText::new(format!(
                "Realtime drops: {} in / {} out",
                status.realtime_ingress_dropped, status.realtime_egress_dropped
            ))
            .small()
            .color(color),
        );
    });

    if !state.diagnostics.is_empty() {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("Diagnostics ({})", state.diagnostics.len()))
            .default_open(
                matches!(status.room_link, LinkState::Failed | LinkState::Refused)
                    || matches!(status.board_link, LinkState::Failed | LinkState::Refused),
            )
            .show(ui, |ui| {
                for message in &state.diagnostics {
                    ui.label(egui::RichText::new(message).small().color(MUTED));
                }
                if ui.small_button("Clear").clicked() {
                    state.diagnostics.clear();
                }
            });
    }
}

fn receive_events(state: &mut EditorState, bridge: &BridgeHandle) {
    while let Some(event) = bridge.try_event() {
        match event {
            BridgeEvent::TrustRequired(hello) => state.pending_trust = Some(hello),
            BridgeEvent::Diagnostic(message) => state.diagnostic(message),
            BridgeEvent::ProtocolRefused { reason, .. } => {
                state.diagnostic(format!("Protocol refused: {reason}"));
            }
            BridgeEvent::RoomSelected(room) => state.room = room,
            BridgeEvent::BoardDiscovered(board) => {
                if let Some(previous) = state
                    .boards
                    .iter_mut()
                    .find(|previous| previous.address == board.address)
                {
                    *previous = board;
                } else {
                    state.boards.push(board);
                }
            }
            BridgeEvent::Status(status) => {
                if status.board_link == LinkState::Ready {
                    state.pending_trust = None;
                }
            }
            BridgeEvent::RoundTable(_) | BridgeEvent::PitchSet(_) => {}
        }
    }
}

fn card(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(egui::Stroke::new(1.0, PANEL_STROKE))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(12))
        .show(ui, contents);
}

fn section_title(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(17.0).strong());
        ui.label(egui::RichText::new(subtitle).small().color(MUTED));
    });
}

fn status_pill(ui: &mut egui::Ui, label: &str, state: LinkState) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(egui::Stroke::new(1.0, link_color(state)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("{label}  {}", link_label(state)))
                    .small()
                    .strong()
                    .color(link_color(state)),
            );
        });
}

fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .strong()
            .color(egui::Color32::BLACK),
    )
    .fill(ACCENT)
}

fn command(state: &mut EditorState, bridge: &BridgeHandle, command: BridgeCommand) {
    if let Err(error) = bridge.try_command(command) {
        state.diagnostic(error.to_string());
    }
}

fn join_room(state: &mut EditorState, params: &TuttiBridgeParams, bridge: &BridgeHandle) {
    let room = state.room.trim().to_owned();
    if let Ok(mut saved) = params.room_name.lock() {
        *saved = room.clone();
    }
    command(state, bridge, BridgeCommand::SelectRoom(room));
}

fn trust_board(
    state: &mut EditorState,
    params: &TuttiBridgeParams,
    bridge: &BridgeHandle,
    hello: PeerHello,
) {
    let identity = *hello.identity.as_bytes();
    match bridge.try_command(BridgeCommand::TrustBoard(identity)) {
        Ok(()) => {
            if let Ok(mut trusted) = params.trusted_boards.lock()
                && !trusted.contains(&identity)
            {
                trusted.push(identity);
            }
            state.pending_trust = None;
        }
        Err(error) => state.diagnostic(error.to_string()),
    }
}

fn param_checkbox(ui: &mut egui::Ui, setter: &ParamSetter, parameter: &BoolParam) {
    let mut value = parameter.value();
    if ui.checkbox(&mut value, parameter.name()).changed() {
        setter.begin_set_parameter(parameter);
        setter.set_parameter(parameter, value);
        setter.end_set_parameter(parameter);
    }
}

fn link_label(state: LinkState) -> &'static str {
    match state {
        LinkState::Offline => "offline",
        LinkState::Discovering => "discovering",
        LinkState::Connecting => "connecting",
        LinkState::Authenticating => "authenticating",
        LinkState::Repairing => "repairing",
        LinkState::Ready => "ready",
        LinkState::Refused => "protocol refused",
        LinkState::Failed => "failed",
    }
}

fn link_color(state: LinkState) -> egui::Color32 {
    match state {
        LinkState::Ready => ACCENT,
        LinkState::Discovering
        | LinkState::Connecting
        | LinkState::Authenticating
        | LinkState::Repairing => egui::Color32::from_rgb(247, 198, 96),
        LinkState::Refused | LinkState::Failed => egui::Color32::from_rgb(246, 111, 119),
        LinkState::Offline => MUTED,
    }
}

fn short_identity(hello: PeerHello) -> String {
    use std::fmt::Write;

    hello
        .identity
        .as_bytes()
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}
