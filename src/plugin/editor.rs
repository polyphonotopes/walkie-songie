//! egui-based plugin editor with QR code display.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, EguiState};

use crate::words::{generate_room_name, parse_room_input};

use super::{NetCommand, WalkieSongieParams};

const EDITOR_WIDTH: u32 = 300;
const EDITOR_HEIGHT: u32 = 400;

/// Editor state that persists between UI opens.
pub struct EditorState {
    /// Text input for custom channel address
    channel_input: String,
    /// Error message to display
    error_message: Option<String>,
    /// Cached QR code texture
    qr_texture: Option<egui::TextureHandle>,
    /// Channel that the cached QR is for
    qr_channel: String,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            channel_input: String::new(),
            error_message: None,
            qr_texture: None,
            qr_channel: String::new(),
        }
    }
}

/// The walkie-songie plugin editor.
pub struct WalkieSongieEditor {
    params: Arc<WalkieSongieParams>,
    connected_peers: Arc<Mutex<usize>>,
    peer_id: Arc<Mutex<Option<String>>>,
    net_tx: Option<Sender<NetCommand>>,
    egui_state: Arc<EguiState>,
}

impl WalkieSongieEditor {
    pub fn new(
        params: Arc<WalkieSongieParams>,
        connected_peers: Arc<Mutex<usize>>,
        peer_id: Arc<Mutex<Option<String>>>,
        net_tx: Option<Sender<NetCommand>>,
    ) -> Self {
        Self {
            params,
            connected_peers,
            peer_id,
            net_tx,
            egui_state: EguiState::from_size(EDITOR_WIDTH, EDITOR_HEIGHT),
        }
    }
}

impl Editor for WalkieSongieEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let params = self.params.clone();
        let connected_peers = self.connected_peers.clone();
        let peer_id = self.peer_id.clone();
        let net_tx = self.net_tx.clone();

        let editor = create_egui_editor(
            self.egui_state.clone(),
            EditorState::default(),
            |_, _| {},
            move |egui_ctx, _setter, state| {
                draw_editor(egui_ctx, state, &params, &connected_peers, &peer_id, &net_tx);
            },
        );

        editor
            .expect("Failed to create egui editor")
            .spawn(parent, context)
    }

    fn size(&self) -> (u32, u32) {
        self.egui_state.size()
    }

    fn set_scale_factor(&self, _factor: f32) -> bool {
        // EguiState handles scaling internally
        true
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}

    fn param_values_changed(&self) {}
}

/// Draw the editor UI.
fn draw_editor(
    ctx: &egui::Context,
    state: &mut EditorState,
    params: &WalkieSongieParams,
    connected_peers: &Mutex<usize>,
    peer_id_mutex: &Mutex<Option<String>>,
    net_tx: &Option<Sender<NetCommand>>,
) {
    let current_channel = params.get_channel();
    let peer_id = peer_id_mutex.lock().ok().and_then(|p| p.clone());

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.heading("Walkie Songie");
            ui.add_space(8.0);

            // Connection status
            let peers = *connected_peers.lock().unwrap();
            let status_text = if peers == 0 {
                if peer_id.is_some() {
                    "P2P ready, waiting for peers...".to_string()
                } else {
                    "Connecting...".to_string()
                }
            } else {
                format!("{} peer{} connected", peers, if peers == 1 { "" } else { "s" })
            };
            ui.label(status_text);
            ui.add_space(8.0);

            // Current channel display
            ui.label("Channel:");
            ui.monospace(&current_channel);
            ui.add_space(4.0);

            // Shareable link with peer ID
            let shareable = if let Some(pid) = &peer_id {
                format!("{}@{}", current_channel, pid)
            } else {
                current_channel.clone()
            };

            ui.horizontal(|ui| {
                ui.label("Share:");
                if ui.small_button("📋 Copy Link").clicked() {
                    let url = format!("https://polyphonotopes.github.io/walkie-songie/#{}", shareable);
                    ctx.copy_text(url);
                }
            });
            ui.add_space(8.0);

            // QR Code (includes peer ID)
            let qr_size = 150.0;
            draw_qr_code(ui, ctx, state, &shareable, qr_size);
            ui.add_space(8.0);

            // Shuffle button
            if ui.button("🎲 Shuffle Channel").clicked() {
                let new_channel = generate_room_name();
                params.set_channel(new_channel.clone());
                state.channel_input = new_channel.clone();
                state.error_message = None;

                if let Some(tx) = net_tx {
                    let _ = tx.try_send(NetCommand::JoinChannel(new_channel));
                }
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            // Custom channel input (accepts URLs too)
            ui.label("Join room (or paste URL):");
            ui.horizontal(|ui| {
                let response = ui.text_edit_singleline(&mut state.channel_input);

                if ui.button("Join").clicked() || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    let input = state.channel_input.trim();
                    if input.is_empty() {
                        state.error_message = Some("Enter a channel or paste URL".to_string());
                    } else if let Some(room_with_peer) = parse_room_input(input) {
                        // Extract just the room name (before @) for storing
                        let room_name = room_with_peer.split('@').next().unwrap_or(&room_with_peer).to_string();
                        params.set_channel(room_name);
                        state.error_message = None;

                        if let Some(tx) = net_tx {
                            // Send the full room@peer for bootstrap
                            let _ = tx.try_send(NetCommand::JoinChannel(room_with_peer));
                        }
                    } else {
                        state.error_message = Some("Invalid format".to_string());
                    }
                }
            });

            // Error message
            if let Some(err) = &state.error_message {
                ui.colored_label(egui::Color32::RED, err);
            }
        });
    });
}

/// Generate and draw the QR code.
fn draw_qr_code(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut EditorState,
    channel: &str,
    size: f32,
) {
    // Regenerate QR if channel changed
    if state.qr_channel != channel || state.qr_texture.is_none() {
        state.qr_texture = generate_qr_texture(ctx, channel);
        state.qr_channel = channel.to_string();
    }

    if let Some(texture) = &state.qr_texture {
        ui.image(egui::load::SizedTexture::new(
            texture.id(),
            egui::vec2(size, size),
        ));
    } else {
        // Fallback if QR generation fails
        ui.allocate_space(egui::vec2(size, size));
        ui.label("QR Error");
    }
}

/// Generate a QR code as an egui texture.
fn generate_qr_texture(ctx: &egui::Context, channel: &str) -> Option<egui::TextureHandle> {
    use qrcode::QrCode;

    // Create URL for the QR code
    let url = format!("https://polyphonotopes.github.io/walkie-songie/#{}", channel);

    let code = QrCode::new(url.as_bytes()).ok()?;

    // Convert QR to pixel data
    let qr_image = code.render::<image::Luma<u8>>()
        .min_dimensions(256, 256)
        .max_dimensions(256, 256)
        .build();

    let width = qr_image.width() as usize;
    let height = qr_image.height() as usize;

    // Convert grayscale to RGBA
    let pixels: Vec<egui::Color32> = qr_image
        .into_iter()
        .map(|v| {
            if *v < 128 {
                egui::Color32::BLACK
            } else {
                egui::Color32::WHITE
            }
        })
        .collect();

    let color_image = egui::ColorImage {
        size: [width, height],
        pixels,
    };

    Some(ctx.load_texture(
        format!("qr_{}", channel),
        color_image,
        egui::TextureOptions::NEAREST,
    ))
}
