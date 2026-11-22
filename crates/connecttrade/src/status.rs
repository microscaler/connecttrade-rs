//! Whether an order is live, answered once for every caller (#11).

use crate::models::NormalizedStatus;

impl NormalizedStatus {
    /// The order can still fill or change: pending, working, partly filled, or a cancel or replace on its way -
    /// **and any status the spec does not list**. Unknown is treated as live until proven final, so a status Connect
    /// Trade adds never makes a caller stop protecting a working order.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.is_final()
    }

    /// The order is done: filled, cancelled, rejected, expired, replaced by another, or done for the day.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            NormalizedStatus::Filled
                | NormalizedStatus::Cancelled
                | NormalizedStatus::Rejected
                | NormalizedStatus::Expired
                | NormalizedStatus::Replaced
                | NormalizedStatus::DoneForDay
        )
    }

    /// A status the spec lists; false for `Unknown`, which a caller should report.
    #[must_use]
    pub fn is_known(&self) -> bool {
        !matches!(self, NormalizedStatus::Unknown(_))
    }
}

#[cfg(test)]
mod tests {
    use crate::models::NormalizedStatus as S;

    #[test]
    fn every_status_has_an_answer_and_unknown_is_live() {
        for s in [S::Pending, S::Working, S::PartiallyFilled, S::PendingCancel, S::PendingReplace] {
            assert!(s.is_open() && !s.is_final() && s.is_known(), "{s:?}");
        }
        for s in [S::Filled, S::Cancelled, S::Rejected, S::Expired, S::Replaced, S::DoneForDay] {
            assert!(!s.is_open() && s.is_final() && s.is_known(), "{s:?}");
        }
        let held: S = serde_json::from_str("\"held\"").unwrap();
        assert!(held.is_open() && !held.is_final() && !held.is_known());
    }
}
