//! Bounded, reversible window-local acknowledgement of input intent.
//!
//! This never materializes or writes a `SharedPitchSet`. It records which
//! controls are awaiting a worker outcome so presentation can derive one
//! reversible effective view without pretending canonical membership changed.

use serde::{Deserialize, Serialize};

use crate::tuning::TunedDegree;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub(crate) struct PerformanceIntentToken {
    pub generation: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PerformanceFeedbackResolution {
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PerformanceFeedbackEvent {
    Begin {
        token: PerformanceIntentToken,
        target: TunedDegree,
        desired_active: bool,
    },
    CommitBegin {
        token: PerformanceIntentToken,
    },
    RollbackBegin {
        token: PerformanceIntentToken,
    },
    Resolved {
        token: PerformanceIntentToken,
        resolution: PerformanceFeedbackResolution,
    },
    Reset {
        generation: u64,
    },
    InstallGeneration {
        generation: u64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PendingFeedback {
    token: PerformanceIntentToken,
    target: TunedDegree,
    desired_active: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BeginRollback {
    token: PerformanceIntentToken,
    slot: usize,
    previous: Option<PendingFeedback>,
}

pub(crate) struct PerformanceFeedback<const CAPACITY: usize> {
    generation: Option<u64>,
    high_water: u64,
    slots: [Option<PendingFeedback>; CAPACITY],
    begin_rollback: Option<BeginRollback>,
}

impl<const CAPACITY: usize> Default for PerformanceFeedback<CAPACITY> {
    fn default() -> Self {
        Self {
            generation: None,
            high_water: 0,
            slots: [None; CAPACITY],
            begin_rollback: None,
        }
    }
}

impl<const CAPACITY: usize> PerformanceFeedback<CAPACITY> {
    pub(crate) fn apply(&mut self, event: PerformanceFeedbackEvent) -> Result<bool, String> {
        match event {
            PerformanceFeedbackEvent::Begin {
                token,
                target,
                desired_active,
            } => {
                self.begin_transaction(token, target, desired_active)?;
                Ok(true)
            }
            PerformanceFeedbackEvent::CommitBegin { token } => {
                self.commit_begin(token)?;
                Ok(false)
            }
            PerformanceFeedbackEvent::RollbackBegin { token } => self.rollback_begin(token),
            PerformanceFeedbackEvent::Resolved { token, resolution } => {
                Ok(self.resolve(token, resolution))
            }
            PerformanceFeedbackEvent::Reset { generation } => self.reset_generation(generation),
            PerformanceFeedbackEvent::InstallGeneration { generation } => {
                self.install_generation(generation)
            }
        }
    }

    pub(crate) fn begin(
        &mut self,
        token: PerformanceIntentToken,
        target: TunedDegree,
        desired_active: bool,
    ) -> Result<(), String> {
        self.begin_transaction(token, target, desired_active)?;
        self.commit_begin(token)
    }

    fn begin_transaction(
        &mut self,
        token: PerformanceIntentToken,
        target: TunedDegree,
        desired_active: bool,
    ) -> Result<(), String> {
        if self.begin_rollback.is_some() {
            return Err("performance feedback begin transaction is already open".into());
        }
        if token.sequence == 0 {
            return Err("performance intent sequence must be non-zero".into());
        }
        if self.generation != Some(token.generation) {
            return Err("performance intent belongs to a generation that is not installed".into());
        }
        if token.sequence <= self.high_water {
            return Err("performance intent sequence did not increase".into());
        }
        let slot_index = if let Some(index) = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|pending| pending.target == target)
        }) {
            index
        } else {
            self.slots
                .iter()
                .position(Option::is_none)
                .ok_or("performance feedback capacity is full")?
        };
        let previous = self.slots[slot_index];
        self.slots[slot_index] = Some(PendingFeedback {
            token,
            target,
            desired_active,
        });
        self.high_water = token.sequence;
        self.begin_rollback = Some(BeginRollback {
            token,
            slot: slot_index,
            previous,
        });
        Ok(())
    }

    pub(crate) fn commit_begin(&mut self, token: PerformanceIntentToken) -> Result<(), String> {
        let rollback = self
            .begin_rollback
            .take()
            .ok_or("performance feedback begin transaction is not open")?;
        if rollback.token != token {
            self.begin_rollback = Some(rollback);
            return Err("performance feedback commit token does not match open begin".into());
        }
        Ok(())
    }

    pub(crate) fn rollback_begin(&mut self, token: PerformanceIntentToken) -> Result<bool, String> {
        let rollback = self
            .begin_rollback
            .take()
            .ok_or("performance feedback begin transaction is not open")?;
        if rollback.token != token {
            self.begin_rollback = Some(rollback);
            return Err("performance feedback rollback token does not match open begin".into());
        }
        self.slots[rollback.slot] = rollback.previous;
        Ok(true)
    }

    /// Consume only the exact outcome. A late outcome from an older intent or
    /// worker generation cannot clear a newer press.
    pub(crate) fn resolve(
        &mut self,
        token: PerformanceIntentToken,
        _resolution: PerformanceFeedbackResolution,
    ) -> bool {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|pending| pending.token == token))
        else {
            return false;
        };
        *slot = None;
        true
    }

    pub(crate) fn reset_generation(&mut self, generation: u64) -> Result<bool, String> {
        if self.generation != Some(generation) {
            return Err(
                "performance feedback reset belongs to a generation that is not installed".into(),
            );
        }
        let changed = self.slots.iter().any(Option::is_some);
        self.begin_rollback = None;
        self.slots.fill(None);
        Ok(changed)
    }

    pub(crate) fn install_generation(&mut self, generation: u64) -> Result<bool, String> {
        if generation == 0 {
            return Err("performance feedback generation must be non-zero".into());
        }
        if self.generation.is_some_and(|current| generation <= current) {
            return Err("performance feedback generation did not increase".into());
        }
        let changed = self.generation != Some(generation) || self.slots.iter().any(Option::is_some);
        self.generation = Some(generation);
        self.high_water = 0;
        self.begin_rollback = None;
        self.slots.fill(None);
        Ok(changed)
    }

    /// Targets with an input gesture awaiting its exact worker outcome.
    ///
    /// Both add and remove gestures are included because this is a visibly
    /// distinct input-acknowledgement facet, not optimistic room membership.
    pub(crate) fn pending_pressed_degrees(&self) -> Vec<TunedDegree> {
        let mut degrees: Vec<_> = self
            .slots
            .iter()
            .flatten()
            .map(|pending| pending.target)
            .collect();
        degrees.sort();
        degrees.dedup();
        degrees
    }

    /// Latest membership-shaped presentation prediction for each pending
    /// target. This is a read-only input to the local effective view; it must
    /// never author room state, MIDI, carrier frames, or durable records.
    pub(crate) fn pending_membership_predictions(&self) -> Vec<(TunedDegree, bool)> {
        let mut predictions: Vec<_> = self
            .slots
            .iter()
            .flatten()
            .map(|pending| (pending.target, pending.desired_active))
            .collect();
        predictions.sort_by_key(|(target, _)| *target);
        predictions
    }

    /// The latest desired membership for a target, used only to interpret a
    /// subsequent user tap while that earlier intent is still pending.
    pub(crate) fn desired(&self, target: &TunedDegree) -> Option<bool> {
        self.slots
            .iter()
            .flatten()
            .find(|pending| &pending.target == target)
            .map(|pending| pending.desired_active)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuning::Tuning;

    fn degree(index: u16) -> TunedDegree {
        TunedDegree::new(&Tuning::twelve_tet(), index).unwrap()
    }

    #[test]
    fn exact_outcome_cannot_clear_a_newer_intent() {
        let mut feedback = PerformanceFeedback::<4>::default();
        feedback.install_generation(7).unwrap();
        let old = PerformanceIntentToken {
            generation: 7,
            sequence: 1,
        };
        let new = PerformanceIntentToken {
            generation: 7,
            sequence: 2,
        };
        feedback.begin(old, degree(3), true).unwrap();
        feedback.begin(new, degree(3), false).unwrap();
        assert!(!feedback.resolve(old, PerformanceFeedbackResolution::Accepted));
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback.pending_pressed_degrees(), vec![degree(3)]);
        assert_eq!(feedback.desired(&degree(3)), Some(false));
    }

    #[test]
    fn rejected_outer_enqueue_restores_the_replaced_same_target_intent() {
        let mut feedback = PerformanceFeedback::<2>::default();
        feedback.install_generation(7).unwrap();
        let old = PerformanceIntentToken {
            generation: 7,
            sequence: 1,
        };
        let rejected = PerformanceIntentToken {
            generation: 7,
            sequence: 2,
        };
        feedback.begin(old, degree(3), true).unwrap();
        feedback
            .apply(PerformanceFeedbackEvent::Begin {
                token: rejected,
                target: degree(3),
                desired_active: false,
            })
            .unwrap();
        assert_eq!(feedback.desired(&degree(3)), Some(false));
        feedback
            .apply(PerformanceFeedbackEvent::RollbackBegin { token: rejected })
            .unwrap();

        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback.desired(&degree(3)), Some(true));
        assert!(feedback.resolve(old, PerformanceFeedbackResolution::Accepted));
        assert!(!feedback.resolve(rejected, PerformanceFeedbackResolution::Rejected));
    }

    #[test]
    fn rejection_and_explicit_generation_reset_clear_reversibly() {
        let mut feedback = PerformanceFeedback::<4>::default();
        feedback.install_generation(7).unwrap();
        let rejected = PerformanceIntentToken {
            generation: 7,
            sequence: 1,
        };
        feedback.begin(rejected, degree(1), true).unwrap();
        assert!(feedback.resolve(rejected, PerformanceFeedbackResolution::Rejected));
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 7,
                    sequence: 2,
                },
                degree(2),
                true,
            )
            .unwrap();
        assert!(feedback.reset_generation(7).unwrap());
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 7,
                    sequence: 3,
                },
                degree(3),
                true,
            )
            .unwrap();
        assert!(feedback.install_generation(8).unwrap());
        assert_eq!(feedback.len(), 0);
    }

    #[test]
    fn latest_target_wins_remove_is_acknowledged_and_sequences_never_regress() {
        let mut feedback = PerformanceFeedback::<2>::default();
        feedback.install_generation(4).unwrap();
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 4,
                    sequence: 1,
                },
                degree(5),
                true,
            )
            .unwrap();
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 4,
                    sequence: 2,
                },
                degree(5),
                false,
            )
            .unwrap();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback.pending_pressed_degrees(), vec![degree(5)]);
        assert_eq!(feedback.desired(&degree(5)), Some(false));
        assert_eq!(
            feedback.pending_membership_predictions(),
            vec![(degree(5), false)]
        );
        assert!(
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 4,
                        sequence: 2,
                    },
                    degree(6),
                    true,
                )
                .is_err()
        );
    }

    #[test]
    fn fixed_capacity_refuses_without_eviction() {
        let mut feedback = PerformanceFeedback::<2>::default();
        feedback.install_generation(1).unwrap();
        for sequence in 1..=2 {
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 1,
                        sequence,
                    },
                    degree(sequence as u16),
                    true,
                )
                .unwrap();
        }
        assert!(
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 1,
                        sequence: 3,
                    },
                    degree(3),
                    true,
                )
                .is_err()
        );
        assert_eq!(feedback.len(), 2);
    }

    #[test]
    fn delayed_old_generation_begin_cannot_clear_current_state() {
        let mut feedback = PerformanceFeedback::<2>::default();
        feedback.install_generation(9).unwrap();
        let current = PerformanceIntentToken {
            generation: 9,
            sequence: 1,
        };
        feedback.begin(current, degree(4), true).unwrap();

        assert!(
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 8,
                        sequence: 99,
                    },
                    degree(7),
                    false,
                )
                .is_err()
        );
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback.pending_pressed_degrees(), vec![degree(4)]);
        assert_eq!(feedback.desired(&degree(4)), Some(true));
    }

    #[test]
    fn generation_change_requires_explicit_reset() {
        let mut feedback = PerformanceFeedback::<2>::default();
        assert!(
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 1,
                        sequence: 1,
                    },
                    degree(1),
                    true,
                )
                .is_err()
        );
        feedback.install_generation(1).unwrap();
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 1,
                    sequence: 1,
                },
                degree(1),
                true,
            )
            .unwrap();
        assert!(feedback.install_generation(2).unwrap());
        assert_eq!(feedback.len(), 0);
    }

    #[test]
    fn same_generation_reset_clears_slots_but_never_reuses_sequence() {
        let mut feedback = PerformanceFeedback::<2>::default();
        feedback.install_generation(3).unwrap();
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 3,
                    sequence: 8,
                },
                degree(1),
                true,
            )
            .unwrap();
        assert!(feedback.reset_generation(3).unwrap());
        assert!(
            feedback
                .begin(
                    PerformanceIntentToken {
                        generation: 3,
                        sequence: 8,
                    },
                    degree(2),
                    false,
                )
                .is_err()
        );
        feedback
            .begin(
                PerformanceIntentToken {
                    generation: 3,
                    sequence: 9,
                },
                degree(2),
                false,
            )
            .unwrap();
    }
}
