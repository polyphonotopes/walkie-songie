//! Dominator UI components for the voice input application.

use std::sync::Arc;

use dominator::{clone, events, html, Dom};
use futures_signals::signal::SignalExt;
use wasm_bindgen::JsCast;
use web_sys::HtmlTextAreaElement;

use crate::room::RoomState;
use crate::tuning::{parse_scl, Tuning};

use super::app::AppState;
use super::keyboard::{sync_active_pitches, update_tuning};

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
            state.room.lock_mut().clear_pitches();
            state.voice_pitch.set(None);
            sync_active_pitches(&state);
        }))
    })
}

/// Pitch display component showing current detected pitch.
pub fn pitch_display(state: Arc<AppState>) -> Dom {
    html!("div", {
        .class("pitch-display")
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
                                    _ => "---".to_string(),
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
                                    None => "---".to_string(),
                                })
                            }),
                        ])
                    }))
                })))
            }),

            // Confidence indicator
            html!("div", {
                .class("confidence-level")
                .child_signal(state.current_pitch.signal_cloned().map(|event| {
                    Some(html!("div", {
                        .children(&mut [
                            html!("span", {
                                .class("level-label")
                                .text("Confidence: ")
                            }),
                            html!("div", {
                                .class("level-bar")
                                .style_signal("width", futures_signals::signal::always(
                                    match event {
                                        Some(e) => format!("{}%", (e.confidence * 100.0).clamp(0.0, 100.0)),
                                        None => "0%".to_string(),
                                    }
                                ))
                            }),
                        ])
                    }))
                }))
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
