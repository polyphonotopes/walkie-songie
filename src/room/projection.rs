//! Ephemeral application projection of a capability-admitted Room-v5 view.
//!
//! This module deliberately contains no merge algorithm, operation log, network
//! protocol, signatures, or authority decisions. A [`RoomProjection`] is just a
//! convenient render/MIDI cache. The Room-v5 Replicas remain the source of
//! durable truth and replace this projection from their materialized views.

use std::collections::{HashMap, HashSet};

use async_broadcast::{Receiver as BroadcastReceiver, Sender as BroadcastSender, broadcast};
use futures::Stream;
use tokio::sync::watch;
use unicode_segmentation::UnicodeSegmentation;

use crate::tuning::PitchClass;

use super::RoomEvent;

const EVENT_CHANNEL_CAPACITY: usize = 256;

const DEFAULT_EMOJIS: &[&str] = &[
    "🪨", "🥜", "🐚", "🌱", "🫟", "🌀", "✳️", "🫯", "🧶", "🐟", "🦠", "🥑",
];

/// A draggable emoji projected at an absolute pitch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub id: String,
    pub pitch: i32,
    pub emoji: String,
}

impl Piece {
    pub fn pitch_class(&self, notes_per_octave: u8) -> u8 {
        (self.pitch - 60).rem_euclid(i32::from(notes_per_octave)) as u8
    }

    pub fn octave(&self) -> i32 {
        (self.pitch - 60).div_euclid(12) + 4
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ProjectionSnapshot {
    pitches: HashSet<PitchClass>,
    pieces: HashMap<String, (i32, String)>,
    voices: HashMap<String, (Option<i32>, Option<PitchClass>)>,
    pieces_locked: bool,
}

/// Mutable render/MIDI cache derived from a Room-v5 Replica view.
///
/// Local mutation methods exist only for input echo and the deliberately
/// disconnected demo mode. Connected hosts submit commands to a Replica and
/// update this value from the resulting materialized snapshot.
pub struct RoomProjection {
    local_actor: String,
    shared_pitches: HashSet<PitchClass>,
    pieces: HashMap<String, Piece>,
    voices: HashMap<String, (Option<i32>, Option<PitchClass>)>,
    tuning_scl: String,
    pieces_locked: bool,
    available_emojis: Vec<String>,
    notify_tx: watch::Sender<()>,
    notify_rx: watch::Receiver<()>,
    event_tx: BroadcastSender<RoomEvent>,
    _event_rx: BroadcastReceiver<RoomEvent>,
}

impl RoomProjection {
    pub fn new(local_actor: String) -> Self {
        let (notify_tx, notify_rx) = watch::channel(());
        let (mut event_tx, event_rx) = broadcast(EVENT_CHANNEL_CAPACITY);
        event_tx.set_overflow(true);
        Self {
            local_actor,
            shared_pitches: HashSet::new(),
            pieces: HashMap::new(),
            voices: HashMap::new(),
            tuning_scl: String::new(),
            pieces_locked: false,
            available_emojis: default_emojis(),
            notify_tx,
            notify_rx,
            event_tx,
            _event_rx: event_rx,
        }
    }

    pub fn events(&self) -> impl Stream<Item = RoomEvent> + use<> {
        let rx = self.event_tx.new_receiver();
        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.ok().map(|event| (event, rx))
        })
    }

    fn emit(&self, event: RoomEvent) {
        let _ = self.event_tx.try_broadcast(event);
    }

    fn notify(&self) {
        let _ = self.notify_tx.send(());
    }

    fn snapshot(&self) -> ProjectionSnapshot {
        ProjectionSnapshot {
            pitches: self.shared_pitches.clone(),
            pieces: self
                .pieces
                .iter()
                .map(|(id, piece)| (id.clone(), (piece.pitch, piece.emoji.clone())))
                .collect(),
            voices: self.voices.clone(),
            pieces_locked: self.pieces_locked,
        }
    }

    fn emit_diffs(&self, before: &ProjectionSnapshot) -> bool {
        let after = self.snapshot();
        for pitch_class in after.pitches.difference(&before.pitches) {
            self.emit(RoomEvent::PitchAdded {
                pitch_class: *pitch_class,
            });
        }
        for pitch_class in before.pitches.difference(&after.pitches) {
            self.emit(RoomEvent::PitchRemoved {
                pitch_class: *pitch_class,
            });
        }
        for (id, (pitch, emoji)) in &after.pieces {
            match before.pieces.get(id) {
                Some((old_pitch, _)) if old_pitch != pitch => self.emit(RoomEvent::PieceMoved {
                    id: id.clone(),
                    old_pitch: *old_pitch,
                    new_pitch: *pitch,
                }),
                None => self.emit(RoomEvent::PieceAdded {
                    id: id.clone(),
                    pitch: *pitch,
                    emoji: emoji.clone(),
                }),
                _ => {}
            }
        }
        for (id, (pitch, _)) in &before.pieces {
            if !after.pieces.contains_key(id) {
                self.emit(RoomEvent::PieceRemoved {
                    id: id.clone(),
                    pitch: *pitch,
                });
            }
        }
        for (actor, (pitch, pitch_class)) in &after.voices {
            if before.voices.get(actor) != Some(&(*pitch, *pitch_class)) {
                if pitch.is_none() && pitch_class.is_none() {
                    self.emit(RoomEvent::VoiceCleared {
                        peer_id: actor.clone(),
                        pitch: before.voices.get(actor).and_then(|(pitch, _)| *pitch),
                    });
                } else {
                    self.emit(RoomEvent::VoiceChanged {
                        peer_id: actor.clone(),
                        pitch: *pitch,
                        pitch_class: *pitch_class,
                    });
                }
            }
        }
        for (actor, (pitch, _)) in &before.voices {
            if !after.voices.contains_key(actor) {
                self.emit(RoomEvent::VoiceCleared {
                    peer_id: actor.clone(),
                    pitch: *pitch,
                });
            }
        }
        if before.pieces_locked != after.pieces_locked {
            self.emit(RoomEvent::PiecesLockChanged {
                locked: after.pieces_locked,
            });
        }
        before != &after
    }

    /// Replace the cache from an admitted, materialized Room-v5 view.
    pub fn replace_replica_projection(
        &mut self,
        pitches: &[PitchClass],
        pieces: &[Piece],
        voices: &[(String, Option<i32>, Option<PitchClass>)],
        pieces_locked: bool,
        available_emojis: Option<&str>,
    ) {
        let before = self.snapshot();
        let old_emojis = self.available_emojis.clone();
        self.shared_pitches = pitches.iter().copied().collect();
        self.pieces = pieces
            .iter()
            .cloned()
            .map(|piece| (piece.id.clone(), piece))
            .collect();
        self.voices = voices
            .iter()
            .cloned()
            .map(|(actor, pitch, pitch_class)| (actor, (pitch, pitch_class)))
            .collect();
        self.pieces_locked = pieces_locked;
        self.available_emojis = available_emojis
            .filter(|emojis| !emojis.is_empty())
            .map(|emojis| emojis.graphemes(true).map(str::to_owned).collect())
            .unwrap_or_else(default_emojis);
        let changed = self.emit_diffs(&before);
        let emojis_changed = old_emojis != self.available_emojis;
        if emojis_changed {
            self.emit(RoomEvent::EmojisChanged {
                emojis: self.available_emojis.clone(),
            });
        }
        if changed || emojis_changed {
            self.notify();
        }
    }

    pub fn emit_full_state_sync(&self) {
        self.emit(RoomEvent::FullStateSync {
            pitches: self.shared_pitches.iter().copied().collect(),
            pieces: self
                .all_pieces()
                .into_iter()
                .map(|piece| (piece.id, piece.pitch, piece.emoji))
                .collect(),
            voices: self
                .voices
                .iter()
                .map(|(actor, (pitch, pitch_class))| (actor.clone(), *pitch, *pitch_class))
                .collect(),
            pieces_locked: self.pieces_locked,
        });
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.notify_rx.clone()
    }

    pub fn local_peer_id(&self) -> &str {
        &self.local_actor
    }

    pub fn add_pitch(&mut self, pitch_class: PitchClass) {
        if self.shared_pitches.insert(pitch_class) {
            self.emit(RoomEvent::PitchAdded { pitch_class });
            self.notify();
        }
    }

    pub fn remove_pitch(&mut self, pitch_class: PitchClass) {
        if self.shared_pitches.remove(&pitch_class) {
            self.emit(RoomEvent::PitchRemoved { pitch_class });
            self.notify();
        }
    }

    pub fn set_single_pitch(&mut self, pitch_class: PitchClass) {
        let before = self.snapshot();
        self.shared_pitches.clear();
        self.shared_pitches.insert(pitch_class);
        self.emit_diffs(&before);
        self.notify();
    }

    pub fn clear_pitches(&mut self) {
        if !self.shared_pitches.is_empty() {
            self.shared_pitches.clear();
            self.emit(RoomEvent::PitchesCleared);
            self.notify();
        }
    }

    pub fn contains_pitch(&self, pitch_class: PitchClass) -> bool {
        self.shared_pitches.contains(&pitch_class)
    }

    pub fn shared_pitches(&self) -> HashSet<PitchClass> {
        self.shared_pitches.clone()
    }

    pub fn tuning_scl(&self) -> &str {
        &self.tuning_scl
    }

    pub fn get_tuning_scl(&self) -> String {
        self.tuning_scl.clone()
    }

    pub fn set_tuning_scl(&mut self, scl: &str) {
        if self.tuning_scl != scl {
            self.tuning_scl = scl.to_owned();
            self.emit(RoomEvent::TuningChanged {
                scl: self.tuning_scl.clone(),
            });
            self.notify();
        }
    }

    pub fn set_voice_pitch(&mut self, pitch: Option<i32>) {
        let pitch_class = pitch.map(|pitch| PitchClass(pitch.rem_euclid(12) as u16));
        self.set_voice(pitch, pitch_class);
    }

    pub fn set_voice_pitchclass(&mut self, pitch_class: Option<PitchClass>) {
        let pitch = pitch_class.map(|pitch_class| 60 + i32::from(pitch_class.index()));
        self.set_voice(pitch, pitch_class);
    }

    pub fn set_voice(&mut self, pitch: Option<i32>, pitch_class: Option<PitchClass>) {
        let previous = self
            .voices
            .get(&self.local_actor)
            .copied()
            .unwrap_or_default();
        if previous == (pitch, pitch_class) {
            return;
        }
        if pitch.is_none() && pitch_class.is_none() {
            self.voices.remove(&self.local_actor);
            self.emit(RoomEvent::VoiceCleared {
                peer_id: self.local_actor.clone(),
                pitch: previous.0,
            });
        } else {
            self.voices
                .insert(self.local_actor.clone(), (pitch, pitch_class));
            self.emit(RoomEvent::VoiceChanged {
                peer_id: self.local_actor.clone(),
                pitch,
                pitch_class,
            });
        }
        self.notify();
    }

    pub fn clear_voice(&mut self) {
        self.set_voice(None, None);
    }

    pub fn all_voice_states(&self) -> HashMap<String, (Option<i32>, Option<PitchClass>)> {
        self.voices.clone()
    }

    pub fn get_peer_voice(&self, actor: &str) -> (Option<i32>, Option<PitchClass>) {
        self.voices.get(actor).copied().unwrap_or_default()
    }

    pub fn local_voice(&self) -> (Option<i32>, Option<PitchClass>) {
        self.get_peer_voice(&self.local_actor)
    }

    pub fn all_voice_pitch_classes(&self) -> Vec<PitchClass> {
        self.voices
            .values()
            .filter_map(|(_, pitch_class)| *pitch_class)
            .collect()
    }

    pub fn all_voice_pitches(&self) -> HashSet<i32> {
        self.voices
            .values()
            .filter_map(|(pitch, _)| *pitch)
            .collect()
    }

    pub fn clear_voice_at_pitch_class(&mut self, target: PitchClass) -> bool {
        let actors: Vec<_> = self
            .voices
            .iter()
            .filter(|(_, (_, pitch_class))| *pitch_class == Some(target))
            .map(|(actor, _)| actor.clone())
            .collect();
        for actor in &actors {
            let previous = self.voices.remove(actor).and_then(|(pitch, _)| pitch);
            self.emit(RoomEvent::VoiceCleared {
                peer_id: actor.clone(),
                pitch: previous,
            });
        }
        if !actors.is_empty() {
            self.notify();
        }
        !actors.is_empty()
    }

    pub fn has_piece_at(&self, pitch: i32) -> bool {
        self.pieces.values().any(|piece| piece.pitch == pitch)
    }

    pub fn add_piece(&mut self, pitch: i32, emoji: &str) -> Option<String> {
        if self.has_piece_at(pitch) {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.pieces.insert(
            id.clone(),
            Piece {
                id: id.clone(),
                pitch,
                emoji: emoji.to_owned(),
            },
        );
        self.emit(RoomEvent::PieceAdded {
            id: id.clone(),
            pitch,
            emoji: emoji.to_owned(),
        });
        self.notify();
        Some(id)
    }

    pub fn remove_piece(&mut self, id: &str) {
        if let Some(piece) = self.pieces.remove(id) {
            self.emit(RoomEvent::PieceRemoved {
                id: id.to_owned(),
                pitch: piece.pitch,
            });
            self.notify();
        }
    }

    pub fn move_piece(&mut self, id: &str, new_pitch: i32) {
        let Some(piece) = self.pieces.get_mut(id) else {
            return;
        };
        let old_pitch = piece.pitch;
        if old_pitch == new_pitch {
            return;
        }
        piece.pitch = new_pitch;
        self.emit(RoomEvent::PieceMoved {
            id: id.to_owned(),
            old_pitch,
            new_pitch,
        });
        self.notify();
    }

    pub fn all_pieces(&self) -> Vec<Piece> {
        let mut pieces: Vec<_> = self.pieces.values().cloned().collect();
        pieces.sort_by(|left, right| left.id.cmp(&right.id));
        pieces
    }

    pub fn get_piece(&self, id: &str) -> Option<Piece> {
        self.pieces.get(id).cloned()
    }

    pub fn clear_pieces(&mut self) {
        if !self.pieces.is_empty() {
            self.pieces.clear();
            self.emit(RoomEvent::PiecesCleared);
            self.notify();
        }
    }

    pub fn find_piece_by_pitch_class(&self, target: u8, notes_per_octave: u8) -> Option<Piece> {
        self.pieces
            .values()
            .find(|piece| piece.pitch_class(notes_per_octave) == target)
            .cloned()
    }

    pub fn pieces_locked(&self) -> bool {
        self.pieces_locked
    }

    pub fn set_pieces_locked(&mut self, locked: bool) {
        if self.pieces_locked != locked {
            self.pieces_locked = locked;
            self.emit(RoomEvent::PiecesLockChanged { locked });
            self.notify();
        }
    }

    pub fn available_emojis(&self) -> Vec<String> {
        self.available_emojis.clone()
    }

    pub fn add_emoji_to_palette(&mut self, emoji: &str) {
        if !self
            .available_emojis
            .iter()
            .any(|candidate| candidate == emoji)
        {
            self.available_emojis.push(emoji.to_owned());
            self.emit(RoomEvent::EmojisChanged {
                emojis: self.available_emojis.clone(),
            });
            self.notify();
        }
    }
}

fn default_emojis() -> Vec<String> {
    DEFAULT_EMOJIS
        .iter()
        .map(|emoji| (*emoji).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_projection_replaces_state_without_becoming_an_authority() {
        let mut projection = RoomProjection::new("local".to_owned());
        projection.add_pitch(PitchClass(1));
        projection.replace_replica_projection(
            &[PitchClass(5)],
            &[Piece {
                id: "piece".to_owned(),
                pitch: 67,
                emoji: "🌱".to_owned(),
            }],
            &[("remote".to_owned(), Some(69), Some(PitchClass(9)))],
            true,
            Some("🐟🦠"),
        );

        assert_eq!(projection.shared_pitches(), HashSet::from([PitchClass(5)]));
        assert_eq!(projection.all_pieces()[0].id, "piece");
        assert_eq!(
            crate::room::snapshot_active_pitches(&projection).unified_pitch_classes(12),
            HashSet::from([5, 7]),
            "the sounding projection includes the piece without changing the manual facet"
        );
        assert_eq!(projection.get_peer_voice("remote").0, Some(69));
        assert!(projection.pieces_locked());
        assert_eq!(projection.available_emojis(), ["🐟", "🦠"]);
    }
}
