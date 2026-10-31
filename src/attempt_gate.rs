//! Delivery decision for one download attempt (CAS-arbitrated, std only).
//! Deliver only after `try_commit()` wins; `mark_delivered()` after success, `was_delivered()` after completion.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Attempt states; caller performs the external action.
const ACTIVE: u8 = 0;
const COMMITTING: u8 = 1;
const DISCARDED: u8 = 2;

/// Delivery decision for one attempt, shared via cloneable `Arc` handles.
#[derive(Debug)]
pub struct AttemptGate {
    state: AtomicU8,
    delivered: AtomicBool,
}

impl AttemptGate {
    /// Creates a gate in the `ACTIVE` state and returns its shareable handle.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: AtomicU8::new(ACTIVE),
            delivered: AtomicBool::new(false),
        })
    }

    /// Claim the right to deliver; true only on winning ACTIVE->COMMITTING, else must not deliver.
    #[must_use]
    pub fn try_commit(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether the attempt still accepts work before its delivery decision.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == ACTIVE
    }

    /// Claim discard; true only on winning ACTIVE->DISCARDED.
    #[must_use]
    pub fn discard(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, DISCARDED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Record successful delivery; call only after delivery.
    pub fn mark_delivered(&self) {
        self.delivered.store(true, Ordering::Release);
    }

    /// Whether delivery was recorded; read only after the worker completes.
    pub fn was_delivered(&self) -> bool {
        self.delivered.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[path = "attempt_gate_tests.rs"]
mod tests;
