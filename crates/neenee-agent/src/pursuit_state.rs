//! In-memory pursuit state, extracted from the `Agent` god-object.
//!
//! Holds the three pieces of mutable pursuit state that used to be separate
//! `Arc<Mutex<…>>` fields on [`crate::Agent`]:
//!
//! - the active [`Pursuit`] (if any),
//! - whether the stop-gate is armed,
//! - the iteration counter driven by the stop-gate.
//!
//! The [`crate::Agent`] owns a single `PursuitState` and delegates its
//! pursuit-related public methods here, keeping the existing call sites
//! (`agent.get_pursuit()`, `agent.arm_pursuit()`, …) unchanged.

use std::sync::{Arc, Mutex};

use neenee_core::Pursuit;

use crate::pursuits;

/// Internal lock-guard helper: poison-immune (recovers via `into_inner`).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// In-memory runtime view of pursuit state.
///
/// Cheap to construct; all fields are `Arc<Mutex<_>>` so a clone is a shallow
/// share (used to hand the same pursuit state to the pursuit tools' context
/// without a lifetime tie to the agent).
#[derive(Clone)]
pub struct PursuitState {
    pursuit: Arc<Mutex<Option<Pursuit>>>,
    armed: Arc<Mutex<bool>>,
    iterations: Arc<Mutex<u32>>,
}

impl Default for PursuitState {
    fn default() -> Self {
        Self {
            pursuit: Arc::new(Mutex::new(None)),
            armed: Arc::new(Mutex::new(false)),
            iterations: Arc::new(Mutex::new(0)),
        }
    }
}

impl PursuitState {
    pub fn new() -> Self {
        Self::default()
    }

    // ── active pursuit ──────────────────────────────────────────────────

    pub fn get(&self) -> Option<Pursuit> {
        lock(&self.pursuit).clone()
    }

    pub fn set(&self, pursuit: Pursuit) {
        *lock(&self.pursuit) = Some(pursuit);
    }

    pub fn restore(&self, pursuit: Pursuit) {
        *lock(&self.pursuit) = Some(pursuit);
    }

    pub fn clear(&self) {
        *lock(&self.pursuit) = None;
    }

    pub fn can_complete(&self) -> bool {
        self.get().is_some()
    }

    // ── stop-gate ───────────────────────────────────────────────────────

    /// Arm the stop-gate and reset the iteration counter.
    pub fn arm(&self) {
        *lock(&self.iterations) = 0;
        *lock(&self.armed) = true;
    }

    pub fn disarm(&self) {
        *lock(&self.armed) = false;
    }

    pub fn is_armed(&self) -> bool {
        *lock(&self.armed)
    }

    pub fn iterations(&self) -> u32 {
        *lock(&self.iterations)
    }

    /// Increment the iteration counter (called by the turn loops each time
    /// the stop-gate forces another round).
    pub fn bump_iterations(&self) {
        *lock(&self.iterations) += 1;
    }

    // ── continuation logic ──────────────────────────────────────────────

    /// Returns a continuation prompt to force another model round, or `None`
    /// to let the turn end. Consulted by both turn loops just before they
    /// return `TurnOutcome`.
    ///
    /// Returns `Some(prompt)` only when: the gate is armed, an active
    /// (incomplete) pursuit exists, the latest response did not signal
    /// completion (via the marker), and the iteration cap is not exhausted.
    /// Hitting the cap disarms the pursuit and stops.
    pub(crate) fn continuation(
        &self,
        response: &neenee_core::Message,
        max_iterations: u32,
    ) -> Option<String> {
        if !self.is_armed() {
            return None;
        }
        let pursuit = self.get()?;
        if pursuit.is_complete {
            return None;
        }
        if response.content.contains(crate::PURSUIT_COMPLETE_MARKER) {
            return None;
        }
        if self.iterations() >= max_iterations {
            self.disarm();
            return None;
        }
        Some(pursuits::prompts::continuation_prompt(&pursuit))
    }

    /// Append a hidden user message that asks the model to continue the active pursuit.
    pub fn inject_continuation(&self, messages: &mut Vec<neenee_core::Message>) {
        if let Some(pursuit) = self.get() {
            if !pursuit.is_complete {
                messages.push(neenee_core::Message::injected(
                    neenee_core::Role::User,
                    pursuits::prompts::continuation_prompt(&pursuit),
                    neenee_core::InjectionOrigin::new(
                        neenee_core::InjectionKind::PursuitContinuation,
                    ),
                ));
            }
        }
    }

    /// Append a hidden user message that informs the model the pursuit objective changed.
    pub fn inject_objective_updated(&self, messages: &mut Vec<neenee_core::Message>) {
        if let Some(pursuit) = self.get() {
            messages.push(neenee_core::Message::injected(
                neenee_core::Role::User,
                pursuits::prompts::objective_updated_prompt(&pursuit),
                neenee_core::InjectionOrigin::new(
                    neenee_core::InjectionKind::PursuitObjectiveUpdated,
                ),
            ));
        }
    }
}
