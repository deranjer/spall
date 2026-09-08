//! Coarse invalidation counters carried by every job token.
//!
//! A [`Generation`] identifies one loaded world / session. It is bumped whenever
//! the world is reloaded (or a client rejoins a fresh session); every result
//! computed against the previous generation is stale by definition, even if its
//! individual brick revisions happen to line up again at reused coordinates.
//!
//! A [`TopologyEpoch`] is a cheaper signal within one generation: it advances
//! when committed topology changes in a way that can invalidate derived work
//! beyond the exact bricks a job listed as read dependencies (for example a
//! structural relabel that moves cells between components). A job whose epoch no
//! longer matches is stale regardless of its brick revisions.
//!
//! Both are monotonic `u64` values that fail explicitly on exhaustion instead of
//! wrapping, matching the counter convention in `spall_core`.

/// Error from advancing a scheduling counter past `u64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("scheduling counter exhausted")]
pub struct CounterExhausted;

macro_rules! counter {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(pub u64);

        impl $name {
            /// The value a brand-new world / epoch starts at.
            pub const START: Self = Self(0);

            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// The successor value. `Err` at `u64::MAX`; never wraps.
            #[inline]
            pub const fn checked_next(self) -> Result<Self, CounterExhausted> {
                match self.0.checked_add(1) {
                    Some(v) => Ok(Self(v)),
                    None => Err(CounterExhausted),
                }
            }

            /// Advances in place, returning the new value.
            #[inline]
            pub const fn advance(&mut self) -> Result<Self, CounterExhausted> {
                match self.checked_next() {
                    Ok(v) => {
                        *self = v;
                        Ok(v)
                    }
                    Err(e) => Err(e),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

counter!(
    /// Identifies one loaded world / session. Bumped on world reload.
    Generation
);
counter!(
    /// Advances on structural changes that can invalidate derived work beyond a
    /// single brick revision, within one [`Generation`].
    TopologyEpoch
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_advance_without_wrapping() {
        let mut g = Generation::START;
        assert_eq!(g.get(), 0);
        assert_eq!(g.advance(), Ok(Generation(1)));
        assert_eq!(g, Generation(1));

        assert_eq!(
            TopologyEpoch(u64::MAX).checked_next(),
            Err(CounterExhausted)
        );
        let mut e = TopologyEpoch(u64::MAX);
        assert_eq!(e.advance(), Err(CounterExhausted));
        assert_eq!(
            e,
            TopologyEpoch(u64::MAX),
            "a failed advance leaves the value"
        );
    }

    #[test]
    fn generations_order_and_display() {
        assert!(Generation(1) < Generation(2));
        assert_eq!(Generation(7).to_string(), "Generation(7)");
        assert_eq!(TopologyEpoch(3).to_string(), "TopologyEpoch(3)");
    }
}
