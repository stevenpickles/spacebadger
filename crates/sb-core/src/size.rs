//! Size values. Logical and allocated sizes are stored separately as integer
//! byte counts; a missing allocation is an explicit unknown, never zero and
//! never a fallback to the logical size.

/// Allocated bytes for one file, or an explicit unknown.
///
/// Stored as a single `u64` with a sentinel so the node table stays compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Allocated(u64);

impl Allocated {
    pub const UNKNOWN: Self = Self(u64::MAX);

    /// A known allocation. `u64::MAX` is reserved and saturates to one less.
    pub const fn known(bytes: u64) -> Self {
        Self(if bytes == u64::MAX {
            u64::MAX - 1
        } else {
            bytes
        })
    }

    pub const fn get(self) -> Option<u64> {
        if self.0 == u64::MAX {
            None
        } else {
            Some(self.0)
        }
    }

    pub const fn is_unknown(self) -> bool {
        self.0 == u64::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_zero() {
        assert_eq!(Allocated::UNKNOWN.get(), None);
        assert_eq!(Allocated::known(0).get(), Some(0));
        assert_ne!(Allocated::UNKNOWN, Allocated::known(0));
    }

    #[test]
    fn known_never_collides_with_sentinel() {
        assert!(!Allocated::known(u64::MAX).is_unknown());
    }
}
