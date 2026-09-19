use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct CaptureDiagnostics {
    total: u64,
    pending: u64,
    last_report: Option<Instant>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct DiscontinuityReport {
    pub total: u64,
    pub since_last_report: u64,
}

impl CaptureDiagnostics {
    pub(super) fn observe(&mut self, discontinuous: bool, now: Instant) -> Option<DiscontinuityReport> {
        if !discontinuous {
            return None;
        }
        self.total = self.total.saturating_add(1);
        self.pending = self.pending.saturating_add(1);
        if self.last_report.is_some_and(|last| now.duration_since(last) < REPORT_INTERVAL) {
            return None;
        }
        let report = DiscontinuityReport {
            total: self.total,
            since_last_report: self.pending,
        };
        self.pending = 0;
        self.last_report = Some(now);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discontinuity_counts_survive_rate_limiting() {
        let start = Instant::now();
        let mut diagnostics = CaptureDiagnostics::default();
        assert_eq!(diagnostics.observe(false, start), None);
        assert_eq!(diagnostics.observe(true, start), Some(DiscontinuityReport {
            total: 1,
            since_last_report: 1,
        }));
        for millis in 1..5000 {
            assert_eq!(diagnostics.observe(true, start + Duration::from_millis(millis)), None);
        }
        assert_eq!(diagnostics.observe(true, start + REPORT_INTERVAL), Some(DiscontinuityReport {
            total: 5001,
            since_last_report: 5000,
        }));
        assert_eq!(diagnostics.observe(false, start + REPORT_INTERVAL * 2), None);
    }
}
