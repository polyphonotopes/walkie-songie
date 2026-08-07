//! Dominator UI components for the voice input application.

use std::sync::Arc;

use dominator::{Dom, clone, events, html};
use futures_signals::signal::SignalExt;
use wasm_bindgen::JsCast;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};

use crate::tuning::{Tuning, parse_scl};
use crate::words::{generate_room_name, generate_room_qr_svg};

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
            // Connected: `clear_native_musical_state` dispatches the retractions
            // and the projection is the sole writer of `state.room`. Offline:
            // the local adapter is authoritative, so clear it here.
            state.clear_native_musical_state();
            if !state.native_backend {
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
            // Connected: dispatch only; `pieces_locked` and `state.room` both
            // repaint from the projection (RoomConfigChanged). Offline: the
            // local adapter is authoritative, so write it here.
            state.set_native_pieces_locked(new_locked);
            if !state.native_backend {
                state.pieces_locked.set(new_locked);
                state.room.lock_mut().set_pieces_locked(new_locked);
            }
        }))
    })
}

/// Emoji picker component - shows one emoji at a time with prev/next arrows.
/// Drag the displayed emoji onto keyboard keys to add pieces.
pub fn emoji_picker(state: Arc<AppState>) -> Dom {
    // Signal that updates on emoji index changes
    let emoji_signal = state
        .selected_emoji_idx
        .signal()
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
                                        match tuning.quantize(hz) {
                                            Ok(result) => format!(
                                                "{} ({:.1} Hz, {}{:.0}¢)",
                                                tuning.note_name(result.pitch_class),
                                                hz,
                                                if result.cents_deviation >= 0.0 { "+" } else { "" },
                                                result.cents_deviation
                                            ),
                                            Err(_) => String::new(),
                                        }
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
                                if !input.is_empty() {
                                    state.enter_room_or_ticket(input);
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
                                .text_signal(state.room_ticket.signal_cloned().map(|ticket| {
                                    if ticket.is_some() { "📋 Copy Ticket" } else { "📋 Copy Link" }
                                }))
                                .event(clone!(state => move |_: events::Click| {
                                    let room = state.room_name.get_cloned();
                                    if let Some(window) = web_sys::window() {
                                        let link = state.room_ticket.get_cloned().unwrap_or_else(|| {
                                            let base = "https://polyphonotopes.github.io/walkie-songie";
                                            format!("{}#{}", base, room)
                                        });
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
                        .text_signal(if state.native_backend {
                            state.native_status.signal_cloned().boxed()
                        } else {
                            state.iroh_peer_id.signal_cloned().map(|p| {
                                if p.is_some() { "✓ Connected" } else { "⏳ Connecting..." }.to_string()
                            }).boxed()
                        })
                    }),

                    // QR Code (always visible)
                    html!("div", {
                        .class("qr-container")
                        .child_signal(state.room_name.signal_cloned().map(|room_name| {
                            // Use full base URL with hash for room name
                            let base = "https://polyphonotopes.github.io/walkie-songie";
                            let svg = generate_room_qr_svg(&room_name, base);
                            Some(html!("div", {
                                .class("qr-code")
                                .attr("data-room", &room_name)
                                .prop("innerHTML", &svg)
                            }))
                        }))
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
                                        Ok(scale) => {
                                            match Tuning::from_scl("Custom".to_string(), scale, None) {
                                                Ok(tuning) => {
                                                    state.scl_error.set(None);
                                                    // Connected: dispatch only; `state.tuning`, `state.room`
                                                    // and the keyboard display all repaint from the
                                                    // projection (TuningChanged). Offline: the local adapter
                                                    // is authoritative, so apply it here.
                                                    state.set_native_tuning(content.clone());
                                                    if !state.native_backend {
                                                        state.tuning.set(tuning.clone());
                                                        state.room.lock_mut().set_tuning_scl(&content);
                                                        update_tuning(&tuning);
                                                    }
                                                }
                                                Err(e) => state.scl_error.set(Some(e.to_string())),
                                            }
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
                            state.set_native_tuning(crate::tuning::TWELVE_TET_SCL.to_owned());
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
                        .prop_signal("value", state.midi_input_id.signal_cloned().map(|id| id.unwrap_or_default()))
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
                            state.midi_devices_version.signal().map(clone!(state => move |_| {
                                let devices: Vec<(String, String)> = if state.tauri_backend() {
                                    state.native_midi_inputs.lock_ref().iter()
                                        .map(|dev| (dev.id.clone(), dev.name.clone()))
                                        .collect()
                                } else if let Ok(midi) = state.midi.try_borrow() {
                                    midi.available_inputs.iter()
                                        .map(|dev| (dev.id.clone(), dev.name.clone()))
                                        .collect()
                                } else {
                                    vec![]
                                };
                                devices.into_iter().map(|(id, name)| {
                                        html!("option", {
                                            .attr("value", &id)
                                            .text(&name)
                                        })
                                    }).collect::<Vec<_>>()
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
                        .prop_signal("value", state.midi_output_id.signal_cloned().map(|id| id.unwrap_or_default()))
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
                            state.midi_devices_version.signal().map(clone!(state => move |_| {
                                let devices: Vec<(String, String)> = if state.tauri_backend() {
                                    state.native_midi_outputs.lock_ref().iter()
                                        .map(|dev| (dev.id.clone(), dev.name.clone()))
                                        .collect()
                                } else if let Ok(midi) = state.midi.try_borrow() {
                                    midi.available_outputs.iter()
                                        .map(|dev| (dev.id.clone(), dev.name.clone()))
                                        .collect()
                                } else {
                                    vec![]
                                };
                                devices.into_iter().map(|(id, name)| {
                                        html!("option", {
                                            .attr("value", &id)
                                            .text(&name)
                                        })
                                    }).collect::<Vec<_>>()
                            })).to_signal_vec()
                        )
                    }),
                ])
            }),
        ])
    })
}

/// Info panel component for the second page.
/// Shows matching pitch class set names, bass note with solfege, treble note with solfege.
pub fn info_panel(state: Arc<AppState>) -> Dom {
    use super::solfege::{BASS_CLEF, TREBLE_CLEF};
    use futures::future::ready;
    use futures::stream::StreamExt;
    use futures_signals::signal::from_stream;

    // Create signal from room events
    let (initial_data, events) = {
        let room = state.room.lock_ref();
        let tuning = state.tuning.lock_ref();
        let data = compute_info_panel_data(&room, &tuning);
        (data, room.events())
    };

    let state_for_stream = state.clone();
    let state_stream = events
        .filter(|e| ready(e.affects_pitches() || e.affects_voice() || e.affects_pieces()))
        .map(move |_| {
            let room = state_for_stream.room.lock_ref();
            let tuning = state_for_stream.tuning.lock_ref();
            compute_info_panel_data(&room, &tuning)
        });

    let full_stream = futures::stream::once(ready(initial_data)).chain(state_stream);
    let info_signal = from_stream(full_stream).map(|opt| opt.unwrap_or_default());

    html!("div", {
        .class("info-panel")
        .child_signal(info_signal.map(|info: InfoPanelData| {
            Some(html!("div", {
                .class("info-panel-content")
                .children(&mut [
                    // Possibly Solfege section (bass and treble in grid)
                    html!("div", {
                        .class("info-section")
                        .class("solfege-section")
                        .children(&mut [
                            html!("div", {
                                .class("section-label")
                                .text("possibly solfege")
                            }),
                            html!("div", {
                                .class("solfege-grid")
                                .children(&mut [
                                    // Bass clef icon
                                    html!("span", {
                                        .class("clef-icon")
                                        .text(&BASS_CLEF.to_string())
                                    }),
                                    // Bass info
                                    if let Some(ref bass) = info.bass {
                                        html!("div", {
                                            .class("solfege-info")
                                            .children(&mut [
                                                html!("span", {
                                                    .class("note-chip")
                                                    .children(&mut [
                                                        if !bass.source_emoji.is_empty() {
                                                            html!("span", {
                                                                .class("source-emoji")
                                                                .text(&bass.source_emoji)
                                                            })
                                                        } else {
                                                            html!("span", {})
                                                        },
                                                        html!("span", {
                                                            .class("note-name")
                                                            .text(&bass.note_name)
                                                        }),
                                                    ])
                                                }),
                                                html!("div", {
                                                    .class("solfege-list")
                                                    .children(bass.solfeges.iter().map(|s| {
                                                        html!("span", {
                                                            .class("solfege-chip")
                                                            .text(s)
                                                        })
                                                    }).collect::<Vec<_>>())
                                                }),
                                            ])
                                        })
                                    } else {
                                        html!("div", {
                                            .class("solfege-info")
                                            .class("empty")
                                            .text("—")
                                        })
                                    },
                                    // Treble clef icon
                                    html!("span", {
                                        .class("clef-icon")
                                        .text(&TREBLE_CLEF.to_string())
                                    }),
                                    // Treble info
                                    if let Some(ref treble) = info.treble {
                                        html!("div", {
                                            .class("solfege-info")
                                            .children(&mut [
                                                html!("span", {
                                                    .class("note-chip")
                                                    .children(&mut [
                                                        if !treble.source_emoji.is_empty() {
                                                            html!("span", {
                                                                .class("source-emoji")
                                                                .text(&treble.source_emoji)
                                                            })
                                                        } else {
                                                            html!("span", {})
                                                        },
                                                        html!("span", {
                                                            .class("note-name")
                                                            .text(&treble.note_name)
                                                        }),
                                                    ])
                                                }),
                                                html!("div", {
                                                    .class("solfege-list")
                                                    .children(treble.solfeges.iter().map(|s| {
                                                        html!("span", {
                                                            .class("solfege-chip")
                                                            .text(s)
                                                        })
                                                    }).collect::<Vec<_>>())
                                                }),
                                            ])
                                        })
                                    } else {
                                        html!("div", {
                                            .class("solfege-info")
                                            .class("empty")
                                            .text("—")
                                        })
                                    },
                                ])
                            }),
                        ])
                    }),

                    // Compatible with section (scales and chords)
                    html!("div", {
                        .class("info-section")
                        .class("scales-section")
                        .children(&mut [
                            html!("div", {
                                .class("section-label")
                                .text("compatible with")
                            }),
                            if info.scale_names.is_empty() {
                                html!("div", {
                                    .class("empty-state")
                                    .text("—")
                                })
                            } else {
                                html!("div", {
                                    .class("scale-tags")
                                    .children(info.scale_names.iter().map(|(name, group, is_exact)| {
                                        html!("span", {
                                            .class("scale-tag")
                                            .class(group.as_str())
                                            .class_signal("exact", futures_signals::signal::always(*is_exact))
                                            .text(name)
                                        })
                                    }).collect::<Vec<_>>())
                                })
                            }
                        ])
                    }),
                ])
            }))
        }))
    })
}

/// Data for the info panel display
#[derive(Default, Clone)]
struct InfoPanelData {
    /// (name, category, is_exact_match)
    scale_names: Vec<(String, String, bool)>,
    bass: Option<NoteDisplayInfo>,
    treble: Option<NoteDisplayInfo>,
}

/// Display info for a single note
#[derive(Clone)]
struct NoteDisplayInfo {
    note_name: String,
    solfeges: Vec<String>,
    source_emoji: String,
}

/// Compute info panel data from room state
fn compute_info_panel_data(
    room: &crate::room::RoomState,
    tuning: &crate::tuning::Tuning,
) -> InfoPanelData {
    use super::graph::find_matching_scale_names;
    use super::solfege::{NoteSource, analyze_range};
    use crate::room::snapshot_active_pitches;

    // Graph/mode/solfège analysis is factual only for the standard 12-TET
    // mapping it was written for.
    if !tuning.supports_standard_note_names() {
        return InfoPanelData::default();
    }

    // Use canonical unified pitch classes (includes toggles + pieces + voice)
    let snapshot = snapshot_active_pitches(room);
    let pitch_classes: Vec<u8> = snapshot
        .unified_pitch_classes()
        .into_iter()
        .filter_map(|pitch| u8::try_from(pitch).ok())
        .collect();

    // Find matching scales
    let scale_names = find_matching_scale_names(&pitch_classes);

    // Collect all notes with their sources for range analysis
    let mut all_notes: Vec<(i32, super::solfege::NoteSource)> = Vec::new();

    // Add voice pitches
    for &pitch in &snapshot.voice_pitches {
        all_notes.push((pitch, NoteSource::Voice));
    }

    // Add pieces (dragged with their own emoji)
    for piece in room.all_pieces() {
        all_notes.push((piece.pitch, NoteSource::Piece(piece.emoji.clone())));
    }

    // If no notes with octave info, we can't show range
    // But we can show toggle mode pitches as "generic" notes at octave 4
    if all_notes.is_empty() && !pitch_classes.is_empty() {
        // Use pitch classes as notes at octave 4 (MIDI 48-59)
        for pc in &pitch_classes {
            all_notes.push((48 + *pc as i32, NoteSource::Toggle));
        }
    }

    // Analyze range
    let range_info = analyze_range(&all_notes, &pitch_classes);

    let bass = range_info.bass.map(|r| NoteDisplayInfo {
        note_name: r.preferred_name().to_string(),
        solfeges: r.solfeges,
        source_emoji: r.source.emoji(),
    });

    let treble = range_info.treble.map(|r| NoteDisplayInfo {
        note_name: r.preferred_name().to_string(),
        solfeges: r.solfeges,
        source_emoji: r.source.emoji(),
    });

    InfoPanelData {
        scale_names,
        bass,
        treble,
    }
}
