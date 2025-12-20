//! Dominator UI components for the voice input application.

use std::sync::Arc;

use dominator::{clone, events, html, Dom};
use futures_signals::signal::SignalExt;
use wasm_bindgen::JsCast;
use web_sys::{HtmlTextAreaElement, HtmlInputElement};

use crate::tuning::{parse_scl, Tuning};
use crate::words::{generate_room_name, generate_room_qr_svg, is_valid_room_input, parse_room_input};

use super::app::AppState;
use super::keyboard::{start_emoji_drag, sync_active_pitches, update_tuning};

/// Voice input button component.
/// Hold to record, release to commit the detected pitch.
pub fn voice_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("voice-button")
        .class_signal("active", state.voice_active.signal())
        .child(html!("span", {
            .class("voice-emoji")
            .text("🗣️")
        }))
        .child(html!("span", {
            .class("voice-text")
            .text_signal(state.voice_active.signal().map(|active| {
                if active { "..." } else { " Sing" }
            }))
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
            // Sync UI and MIDI output (sends note-offs)
            sync_active_pitches(&state);
            state.sync_midi_toggle_output();
            state.sync_midi_voice_output();
        }))
    })
}

/// Lock button - prevents editing of toggles and pieces.
pub fn lock_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("lock-button")
        .class_signal("locked", state.pieces_locked.signal())
        .text_signal(state.pieces_locked.signal().map(|locked| {
            if locked { "🔒" } else { "🔓" }
        }))
        .attr("title", "Lock/unlock keyboard editing")
        .event(clone!(state => move |_: events::Click| {
            let new_locked = !state.pieces_locked.get();
            state.pieces_locked.set(new_locked);
            // Persist to CRDT
            state.room.lock_mut().set_pieces_locked(new_locked);
        }))
    })
}

/// Emoji picker component - shows one emoji at a time with prev/next arrows.
/// Drag the displayed emoji onto keyboard keys to add pieces.
pub fn emoji_picker(state: Arc<AppState>) -> Dom {
    // Signal that updates on emoji index changes
    let emoji_signal = state.selected_emoji_idx.signal()
        .map(clone!(state => move |idx| {
            let emojis = state.room.lock_ref().available_emojis();
            let count = emojis.len();
            let safe_idx = if count > 0 { idx % count } else { 0 };
            (emojis, safe_idx, count)
        }));

    html!("div", {
        .class("emoji-picker")
        .children(&mut [
            // Prev button
            html!("button", {
                .class("emoji-nav-btn")
                .text("◀")
                .event(clone!(state => move |_: events::Click| {
                    let emojis = state.room.lock_ref().available_emojis();
                    let count = emojis.len();
                    if count > 0 {
                        let current = state.selected_emoji_idx.get();
                        let new_idx = if current == 0 { count - 1 } else { current - 1 };
                        state.selected_emoji_idx.set(new_idx);
                    }
                }))
            }),
            // Current emoji (draggable)
            html!("div", {
                .class("emoji-picker-current")
                .child_signal(emoji_signal.map(move |(emojis, idx, _count): (Vec<String>, usize, usize)| {
                    if emojis.is_empty() {
                        return Some(html!("span", { .text("—") }));
                    }
                    let emoji = emojis[idx].clone();
                    let emoji_clone = emoji.clone();
                    let emoji_drag = emoji.clone();
                    Some(html!("div", {
                        .class("emoji-picker-item")
                        .attr("data-emoji", &emoji)
                        .attr("draggable", "true")
                        .text(&emoji)
                        // HTML5 drag for desktop
                        .event(move |e: events::DragStart| {
                            if let Some(dt) = e.data_transfer() {
                                let _ = dt.set_data("text/plain", &emoji_clone);
                                dt.set_effect_allowed("copy");
                            }
                        })
                        // Pointer events for touch
                        .after_inserted(move |el| {
                            let emoji_for_drag = emoji_drag.clone();
                            let on_down = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                                // Prevent browser from capturing touch for scroll/gestures
                                e.prevent_default();
                                e.stop_propagation();
                                start_emoji_drag(
                                    emoji_for_drag.clone(),
                                    e.pointer_id(),
                                    e.client_x(),
                                    e.client_y(),
                                );
                            });
                            let _ = el.add_event_listener_with_callback(
                                "pointerdown",
                                on_down.as_ref().unchecked_ref(),
                            );
                            on_down.forget();
                        })
                    }))
                }))
            }),
            // Next button
            html!("button", {
                .class("emoji-nav-btn")
                .text("▶")
                .event(clone!(state => move |_: events::Click| {
                    let emojis = state.room.lock_ref().available_emojis();
                    let count = emojis.len();
                    if count > 0 {
                        let current = state.selected_emoji_idx.get();
                        let new_idx = (current + 1) % count;
                        state.selected_emoji_idx.set(new_idx);
                    }
                }))
            }),
        ])
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

/// Room header button - shows satellite dish icon, opens overlay with room details.
pub fn room_header_button(state: Arc<AppState>) -> Dom {
    html!("button", {
        .class("room-header-button")
        .attr("title", "Room settings")
        .text("📡 connect")
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
                                .text("📡 Room Settings")
                            }),
                            html!("button", {
                                .class("close-btn")
                                .attr("aria-label", "Close")
                                .text("✕")
                                .event(clone!(state => move |_: events::Click| {
                                    state.room_overlay_visible.set(false);
                                }))
                            }),
                        ])
                    }),

                    // Room code - full width, editable
                    html!("input" => HtmlInputElement, {
                        .class("room-code-input")
                        .attr("type", "text")
                        .attr("spellcheck", "false")
                        .prop_signal("value", state.room_name.signal_cloned())
                        .event(clone!(state => move |e: events::Input| {
                            if let Some(target) = e.target() {
                                if let Ok(input) = target.dyn_into::<HtmlInputElement>() {
                                    state.room_input.set(input.value());
                                }
                            }
                        }))
                        .event(clone!(state => move |e: events::KeyDown| {
                            if e.key() == "Enter" {
                                let input = state.room_input.get_cloned();
                                if !input.is_empty() && is_valid_room_input(&input) {
                                    if let Some(room_name) = parse_room_input(&input) {
                                        state.set_room_name(room_name);
                                    }
                                }
                            }
                        }))
                    }),

                    // Action buttons row
                    html!("div", {
                        .class("room-actions")
                        .children(&mut [
                            html!("button", {
                                .class("room-action-btn")
                                .text("📋 Copy Link")
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
                                        let clipboard = window.navigator().clipboard();
                                        let _ = clipboard.write_text(&link);
                                    }
                                }))
                            }),
                            html!("button", {
                                .class("room-action-btn")
                                .text("🎲 New Room")
                                .event(clone!(state => move |_: events::Click| {
                                    let new_name = generate_room_name();
                                    state.set_room_name(new_name);
                                }))
                            }),
                        ])
                    }),

                    // Status
                    html!("div", {
                        .class("peer-status")
                        .class_signal("connected", state.iroh_peer_id.signal_cloned().map(|p| p.is_some()))
                        .text_signal(state.iroh_peer_id.signal_cloned().map(|p| {
                            if p.is_some() { "✓ Connected" } else { "⏳ Connecting..." }.to_string()
                        }))
                    }),

                    // QR Code (always visible)
                    html!("div", {
                        .class("qr-container")
                        .child_signal(state.iroh_peer_id.signal_cloned().map(clone!(state => move |peer_id| {
                            let room_name = state.room_name.get_cloned();
                            let origin = web_sys::window()
                                .and_then(|w| w.location().origin().ok())
                                .unwrap_or_else(|| "https://walkie-songie.app".to_string());
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

/// MIDI settings component.
pub fn midi_settings(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("midi-settings")
        .children(&mut [
            html!("div", {
                .class("section-label")
                .text("MIDI")
            }),
            html!("div", {
                .class("midi-row")
                .children(&mut [
                    html!("label", {
                        .attr("for", "midi-input-select")
                        .text("In")
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
                        .text("Out")
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
    })
}
