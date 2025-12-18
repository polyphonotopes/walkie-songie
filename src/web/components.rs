//! Dominator UI components for the voice input application.

use std::sync::Arc;

use dominator::{clone, events, html, Dom};
use futures_signals::signal::SignalExt;
use wasm_bindgen::JsCast;
use web_sys::{HtmlTextAreaElement, HtmlInputElement};

use crate::room::RoomState;
use crate::tuning::{parse_scl, Tuning};
use crate::words::{generate_room_name, generate_room_qr_svg, is_valid_room_name, is_valid_room_input, parse_room_input};

use super::app::AppState;
use super::keyboard::{sync_active_pitches, update_tuning};

/// Join a room by parsing the input from the room-input field.
/// Supports full URLs, room@peer format, or just room names.
/// Reloads the page to switch rooms.
fn join_room_from_input() {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            if let Some(input) = document.get_element_by_id("room-input") {
                if let Ok(input) = input.dyn_into::<HtmlInputElement>() {
                    let value = input.value();
                    if let Some(room_with_peer) = parse_room_input(&value) {
                        // Set hash and reload to join the new room
                        let _ = window.location().set_hash(&room_with_peer);
                        let _ = window.location().reload();
                    }
                }
            }
        }
    }
}

/// Voice input button component.
/// Hold to record, release to commit the detected pitch.
pub fn voice_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("voice-button")
        .class_signal("active", state.voice_active.signal())
        .text_signal(state.voice_active.signal().map(|active| {
            if active { "🗣️ Singing..." } else { "🗣️ Hold to Sing" }
        }))
        .event(clone!(state => move |_: events::PointerDown| {
            state.start_voice();
        }))
        .event(clone!(state => move |_: events::PointerUp| {
            state.stop_voice();
        }))
        .event(clone!(state => move |_: events::PointerLeave| {
            // Also stop if pointer leaves the button while held
            if state.voice_active.get() {
                state.stop_voice();
            }
        }))
    })
}

/// Clear all active pitches button.
pub fn clear_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("clear-button")
        .text("🌊 Clear")
        .event(clone!(state => move |_: events::Click| {
            // Clear both manual pitches and voice pitch
            {
                let mut room = state.room.lock_mut();
                room.clear_pitches();
                room.clear_voice();
                room.clear_pieces();
            }
            state.voice_pitch.set(None);
            // Increment room_version to trigger UI updates
            state.room_version.set(state.room_version.get() + 1);
            sync_active_pitches(&state);
            // Sync MIDI output (sends note-offs)
            state.sync_midi_toggle_output();
            state.sync_midi_voice_output();
        }))
    })
}

/// Piece mode toggle button.
/// Switches between toggle mode (click to toggle notes) and piece mode (click to add pieces).
pub fn piece_mode_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("piece-mode-button")
        .class_signal("active", state.piece_mode.signal())
        .text_signal(state.piece_mode.signal().map(|piece_mode| {
            if piece_mode { "Piece Mode" } else { "Toggle Mode" }
        }))
        .event(clone!(state => move |_: events::Click| {
            let current = state.piece_mode.get();
            state.piece_mode.set(!current);
            // Trigger UI update
            state.room_version.set(state.room_version.get() + 1);
        }))
    })
}

/// Pitch display component showing current detected pitch.
/// Only visible when voice is active (user is holding sing button).
pub fn pitch_display(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("pitch-display")
        .class_signal("hidden", state.voice_active.signal().map(|active| !active))
        .children(&mut [
            // Current pitch indicator (real-time feedback from SwiftF0)
            html!("div", {
                .class("current-pitch")
                .child_signal(state.current_pitch.signal_cloned().map(clone!(state => move |event| {
                    Some(html!("div", {
                        .children(&mut [
                            html!("span", {
                                .class("pitch-label")
                                .text("Detected: ")
                            }),
                            html!("span", {
                                .class("pitch-value")
                                .text(&match event {
                                    Some(e) if e.hz.is_some() => {
                                        let hz = e.hz.unwrap();
                                        let tuning = state.tuning.lock_ref();
                                        let result = tuning.quantize(hz);
                                        format!(
                                            "{} ({:.1} Hz, {}{:.0}¢)",
                                            tuning.note_name(result.pitch_class),
                                            hz,
                                            if result.cents_deviation >= 0.0 { "+" } else { "" },
                                            result.cents_deviation
                                        )
                                    }
                                    _ => "".to_string(),
                                })
                            }),
                        ])
                    }))
                })))
            }),

            // Committed pitch (what will be toggled)
            html!("div", {
                .class("committed-pitch")
                .child_signal(state.committed_pitch.signal().map(clone!(state => move |pc| {
                    Some(html!("div", {
                        .children(&mut [
                            html!("span", {
                                .class("pitch-label")
                                .text("Commit: ")
                            }),
                            html!("span", {
                                .class("pitch-value")
                                .class_signal("has-value", state.committed_pitch.signal().map(|p| p.is_some()))
                                .text(&match pc {
                                    Some(pc) => {
                                        let tuning = state.tuning.lock_ref();
                                        tuning.note_name(pc).to_string()
                                    }
                                    None => "".to_string(),
                                })
                            }),
                        ])
                    }))
                })))
            }),
        ])
    })
}

/// Room header button - shows current room name and opens overlay.
pub fn room_header_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("room-header-button")
        .children(&mut [
            html!("span", {
                .class("qr-icon")
                .text("📡")
            }),
            html!("span", {
                .class("room-name")
                .text_signal(state.room_name.signal_cloned())
            }),
        ])
        .event(clone!(state => move |_: events::Click| {
            state.room_overlay_visible.set(true);
        }))
    })
}

/// Room overlay - shows QR code, room name, and controls.
pub fn room_overlay(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("room-overlay")
        .class_signal("visible", state.room_overlay_visible.signal())
        .children(&mut [
            // Backdrop (click to close)
            html!("div", {
                .class("room-overlay-backdrop")
                .event(clone!(state => move |_: events::Click| {
                    state.room_overlay_visible.set(false);
                }))
            }),

            // Content panel
            html!("div", {
                .class("room-overlay-panel")
                .children(&mut [
                    // Header with close button
                    html!("div", {
                        .class("room-overlay-header")
                        .children(&mut [
                            html!("h2", {
                                .text("Room")
                            }),
                            html!("button", {
                                .class("close-btn")
                                .text("✕")
                                .event(clone!(state => move |_: events::Click| {
                                    state.room_overlay_visible.set(false);
                                }))
                            }),
                        ])
                    }),

                    // Current room display
                    html!("div", {
                        .class("current-room-display")
                        .children(&mut [
                            html!("div", {
                                .class("room-name-large")
                                .text_signal(state.room_name.signal_cloned())
                            }),
                        ])
                    }),

                    // Shareable link with peer ID
                    html!("div", {
                        .class("shareable-link-section")
                        .children(&mut [
                            html!("div", {
                                .class("link-label")
                                .text("Share this link:")
                            }),
                            html!("div", {
                                .class("link-row")
                                .children(&mut [
                                    // Use a signal-driven input that updates when either room or peer ID changes
                                    html!("input", {
                                        .class("link-input")
                                        .attr("type", "text")
                                        .attr("readonly", "")
                                        .attr_signal("value", state.iroh_peer_id.signal_cloned().map(clone!(state => move |peer_id| {
                                            let room = state.room_name.get_cloned();
                                            let origin = web_sys::window()
                                                .and_then(|w| w.location().origin().ok())
                                                .unwrap_or_else(|| "https://walkie-songie.app".to_string());
                                            if let Some(pid) = peer_id {
                                                format!("{}/#{}@{}", origin, room, pid)
                                            } else {
                                                format!("{}/#{}",  origin, room)
                                            }
                                        })))
                                    }),
                                    html!("button", {
                                        .class("copy-btn")
                                        .text("📋 Copy")
                                        .event(clone!(state => move |_: events::Click| {
                                            let room = state.room_name.get_cloned();
                                            let pid = state.iroh_peer_id.get_cloned();
                                            if let Some(window) = web_sys::window() {
                                                let origin = window.location().origin().unwrap_or_else(|_| "https://walkie-songie.app".to_string());
                                                let link = if let Some(p) = pid {
                                                    format!("{}/#{}@{}", origin, room, p)
                                                } else {
                                                    format!("{}/#{}",  origin, room)
                                                };
                                                // Copy to clipboard
                                                let clipboard = window.navigator().clipboard();
                                                let _ = clipboard.write_text(&link);
                                            }
                                        }))
                                    }),
                                ])
                            }),
                            // Status indicator for peer ID
                            html!("div", {
                                .class("peer-status")
                                .class_signal("connected", state.iroh_peer_id.signal_cloned().map(|p| p.is_some()))
                                .text_signal(state.iroh_peer_id.signal_cloned().map(|p| {
                                    if p.is_some() {
                                        "✓ P2P ready - others can join via this link".to_string()
                                    } else {
                                        "⏳ Connecting to P2P network...".to_string()
                                    }
                                }))
                            }),
                        ])
                    }),

                    // QR Code (includes peer ID for bootstrap)
                    html!("div", {
                        .class("qr-container")
                        .child_signal(state.iroh_peer_id.signal_cloned().map(clone!(state => move |peer_id| {
                            let room_name = state.room_name.get_cloned();
                            let origin = web_sys::window()
                                .and_then(|w| w.location().origin().ok())
                                .unwrap_or_else(|| "https://walkie-songie.app".to_string());
                            // Include peer ID in QR code for direct bootstrap
                            let room_with_peer = if let Some(pid) = peer_id {
                                format!("{}@{}", room_name, pid)
                            } else {
                                room_name.clone()
                            };
                            let svg = generate_room_qr_svg(&room_with_peer, &origin);
                            Some(html!("div", {
                                .class("qr-code")
                                .attr("data-room", &room_name)
                                .prop("innerHTML", &svg)
                            }))
                        })))
                    }),

                    // Shuffle button - join new random room
                    html!("button", {
                        .class("shuffle-btn")
                        .text("🎲 New Room")
                        .event(clone!(state => move |_: events::Click| {
                            let new_name = generate_room_name();
                            state.set_room_name(new_name);
                            // TODO: Actually switch rooms via networking
                        }))
                    }),

                    // MIDI settings
                    html!("div", {
                        .class("midi-settings")
                        .children(&mut [
                            html!("div", {
                                .class("midi-row")
                                .children(&mut [
                                    html!("label", {
                                        .attr("for", "midi-input-select")
                                        .text("MIDI In:")
                                    }),
                                    html!("select", {
                                        .attr("id", "midi-input-select")
                                        .class("midi-select")
                                        .event(clone!(state => move |e: events::Change| {
                                            if let Some(target) = e.target() {
                                                if let Ok(select) = target.dyn_into::<web_sys::HtmlSelectElement>() {
                                                    let value = select.value();
                                                    let device_id = if value.is_empty() { None } else { Some(value) };
                                                    state.set_midi_input(device_id);
                                                }
                                            }
                                        }))
                                        .children(&mut [
                                            html!("option", {
                                                .attr("value", "")
                                                .text("None")
                                            }),
                                        ])
                                        .children_signal_vec(
                                            futures_signals::signal::always(()).map(clone!(state => move |_| {
                                                if let Ok(midi) = state.midi.try_borrow() {
                                                    midi.available_inputs.iter().map(|dev| {
                                                        let id = dev.id.clone();
                                                        let name = dev.name.clone();
                                                        html!("option", {
                                                            .attr("value", &id)
                                                            .text(&name)
                                                        })
                                                    }).collect::<Vec<_>>()
                                                } else {
                                                    vec![]
                                                }
                                            })).to_signal_vec()
                                        )
                                    }),
                                ])
                            }),
                            html!("div", {
                                .class("midi-row")
                                .children(&mut [
                                    html!("label", {
                                        .attr("for", "midi-output-select")
                                        .text("MIDI Out:")
                                    }),
                                    html!("select", {
                                        .attr("id", "midi-output-select")
                                        .class("midi-select")
                                        .event(clone!(state => move |e: events::Change| {
                                            if let Some(target) = e.target() {
                                                if let Ok(select) = target.dyn_into::<web_sys::HtmlSelectElement>() {
                                                    let value = select.value();
                                                    let device_id = if value.is_empty() { None } else { Some(value) };
                                                    state.set_midi_output(device_id);
                                                }
                                            }
                                        }))
                                        .children(&mut [
                                            html!("option", {
                                                .attr("value", "")
                                                .text("None")
                                            }),
                                        ])
                                        .children_signal_vec(
                                            futures_signals::signal::always(()).map(clone!(state => move |_| {
                                                if let Ok(midi) = state.midi.try_borrow() {
                                                    midi.available_outputs.iter().map(|dev| {
                                                        let id = dev.id.clone();
                                                        let name = dev.name.clone();
                                                        html!("option", {
                                                            .attr("value", &id)
                                                            .text(&name)
                                                        })
                                                    }).collect::<Vec<_>>()
                                                } else {
                                                    vec![]
                                                }
                                            })).to_signal_vec()
                                        )
                                    }),
                                ])
                            }),
                        ])
                    }),

                    // Manual room entry
                    html!("div", {
                        .class("room-input-section")
                        .children(&mut [
                            html!("label", {
                                .attr("for", "room-input")
                                .text("Join room:")
                            }),
                            html!("div", {
                                .class("room-input-row")
                                .children(&mut [
                                    html!("input" => HtmlInputElement, {
                                        .attr("id", "room-input")
                                        .attr("type", "text")
                                        .attr("placeholder", "sunny-garden-melody or paste full URL")
                                        .class("room-input")
                                        .prop_signal("value", state.room_input.signal_cloned())
                                        .event(clone!(state => move |e: events::Input| {
                                            if let Some(target) = e.target() {
                                                if let Ok(input) = target.dyn_into::<HtmlInputElement>() {
                                                    state.room_input.set(input.value());
                                                }
                                            }
                                        }))
                                        .event(move |e: events::KeyDown| {
                                            if e.key() == "Enter" {
                                                join_room_from_input();
                                            }
                                        })
                                    }),
                                    html!("button", {
                                        .class("join-btn")
                                        .text("Join")
                                        .event(move |_: events::Click| {
                                            join_room_from_input();
                                        })
                                    }),
                                ])
                            }),
                            // Validation feedback
                            html!("div", {
                                .class("room-input-hint")
                                .class_signal("error", state.room_input.signal_cloned().map(|input| {
                                    !input.is_empty() && !is_valid_room_input(&input)
                                }))
                                .text_signal(state.room_input.signal_cloned().map(|input| {
                                    if input.is_empty() {
                                        "Paste URL or enter room name (word-word-word)".to_string()
                                    } else if is_valid_room_input(&input) {
                                        "✓ Valid - press Join to connect".to_string()
                                    } else {
                                        "✗ Invalid format".to_string()
                                    }
                                }))
                            }),
                        ])
                    }),
                ])
            }),
        ])
    })
}

/// Tuning editor component.
pub fn tuning_editor(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("tuning-editor")
        .children(&mut [
            // Current tuning display
            html!("div", {
                .class("tuning-info")
                .child_signal(state.tuning.signal_cloned().map(|tuning| {
                    Some(html!("span", {
                        .text(&format!("{} ({} notes)", tuning.name, tuning.pitch_class_count()))
                    }))
                }))
            }),

            // SCL editor
            html!("div", {
                .class("scl-editor")
                .children(&mut [
                    html!("label", {
                        .attr("for", "scl-input")
                        .text("SCL Tuning (Scala format):")
                    }),
                    html!("textarea", {
                        .attr("id", "scl-input")
                        .attr("rows", "8")
                        .attr("placeholder", "! Comment\nTuning Name\n12\n100.0\n200.0\n...")
                        .class("scl-textarea")
                        .event(clone!(state => move |e: events::Input| {
                            if let Some(target) = e.target() {
                                if let Ok(textarea) = target.dyn_into::<HtmlTextAreaElement>() {
                                    let content = textarea.value();
                                    // Try to parse and update tuning
                                    match parse_scl(&content) {
                                        Ok(cents) => {
                                            let tuning = Tuning::from_scl("Custom".to_string(), cents);
                                            state.tuning.set(tuning.clone());
                                            state.scl_error.set(None);
                                            // Update room SCL
                                            state.room.lock_mut().set_tuning_scl(&content);
                                            // Update keyboard display
                                            update_tuning(&tuning);
                                        }
                                        Err(e) => {
                                            state.scl_error.set(Some(format!("{}", e)));
                                        }
                                    }
                                }
                            }
                        }))
                    }),
                ])
            }),

            // Error display
            html!("div", {
                .class("scl-error")
                .class_signal("visible", state.scl_error.signal_cloned().map(|e| e.is_some()))
                .child_signal(state.scl_error.signal_cloned().map(|error| {
                    error.map(|e| html!("span", { .text(&e) }))
                }))
            }),

            // Preset buttons
            html!("div", {
                .class("tuning-presets")
                .children(&mut [
                    html!("button", {
                        .class("preset-btn")
                        .text("12-TET")
                        .event(clone!(state => move |_: events::Click| {
                            let tuning = Tuning::twelve_tet();
                            state.tuning.set(tuning.clone());
                            state.scl_error.set(None);
                            update_tuning(&tuning);
                        }))
                    }),
                ])
            }),
        ])
    })
}
