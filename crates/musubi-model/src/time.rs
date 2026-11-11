//! Time windows.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A half-open interval of instants, `[start, end)`.
///
/// Used for the initial window around today ([`crate::provider::Start::Window`]),
/// for range queries ([`crate::store::RangeQuery`]) and for coverage of a
/// collection. Construct it with [`TimeWindow::new`], which rejects empty and
/// reversed windows, so every holder may rely on `start < end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimeWindow {
    start: Timestamp,
    end: Timestamp,
}

impl TimeWindow {
    /// Returns `None` unless `start < end`.
    pub fn new(start: Timestamp, end: Timestamp) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    /// Inclusive start.
    pub fn start(&self) -> Timestamp {
        self.start
    }

    /// Exclusive end.
    pub fn end(&self) -> Timestamp {
        self.end
    }

    /// Whether `[start, end)` of an occurrence overlaps this window.
    /// A zero-length occurrence at `t` overlaps when `start <= t < end`.
    pub fn overlaps(&self, start: Timestamp, end: Timestamp) -> bool {
        start < self.end && (end > self.start || (start == end && start >= self.start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_overlaps_half_open() {
        let t = |s: i64| Timestamp::from_second(s).unwrap();
        assert!(TimeWindow::new(t(10), t(10)).is_none());
        assert!(TimeWindow::new(t(11), t(10)).is_none());
        let w = TimeWindow::new(t(10), t(20)).unwrap();
        assert!(w.overlaps(t(5), t(11)));
        assert!(!w.overlaps(t(5), t(10)));
        assert!(!w.overlaps(t(20), t(30)));
        assert!(w.overlaps(t(10), t(10)));
        assert!(!w.overlaps(t(20), t(20)));
    }
}
