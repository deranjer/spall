//! Per-stage timing spans for the owning tick thread (T23 / G4 instrumentation).
//!
//! `Span::start("stage.name")` records its elapsed time into a thread-local list
//! when dropped; the serve loop drains that list once per tick and aggregates it
//! per stage. Nothing here allocates on the hot path beyond a small `Vec` push,
//! and a span on a thread that never drains just accumulates a bounded list.
//!
//! Stages are flat and may nest (an outer span's time includes its children);
//! names are dotted (`commit.plan_collider`) so a reader can tell parents from
//! parts.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Most spans retained per thread between drains (a runaway guard).
const MAX_PENDING: usize = 4096;

thread_local! {
    static PENDING: RefCell<Vec<(&'static str, Duration)>> = const { RefCell::new(Vec::new()) };
}

/// Records the time from [`Span::start`] to drop under `name`.
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    name: &'static str,
    started: Instant,
}

impl Span {
    pub fn start(name: &'static str) -> Self {
        Self {
            name,
            started: Instant::now(),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        PENDING.with(|p| {
            let mut p = p.borrow_mut();
            if p.len() < MAX_PENDING {
                p.push((self.name, elapsed));
            }
        });
    }
}

/// Takes every span recorded on this thread since the last drain.
pub fn drain() -> Vec<(&'static str, Duration)> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_accumulate_and_drain() {
        let _ = drain();
        {
            let _a = Span::start("a");
            let _b = Span::start("b");
        }
        let got = drain();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|(n, _)| *n == "a"));
        assert!(drain().is_empty());
    }
}
