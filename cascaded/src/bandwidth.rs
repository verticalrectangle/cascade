#![allow(dead_code)] // parent wires this into RelayRouter::fan_out
use std::collections::HashMap;

use chrono::{NaiveDate, Utc};

/// Default per-owner daily relay fan-out budget (50 MiB).
const DEFAULT_RELAY_DAILY_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthDecision {
    Allow,
    /// First event that crossed the daily budget; caller injects one Notice.
    Warn,
    Drop,
}

/// In-memory per-owner daily relay byte budget. Day rolls at UTC midnight.
pub struct BandwidthLedger {
    budget: u64,
    accounts: HashMap<String, (u64, NaiveDate, bool)>,
}

impl BandwidthLedger {
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            accounts: HashMap::new(),
        }
    }

    /// Reads `CASCADE_RELAY_DAILY_BYTES` once; default 50 MiB on missing/invalid.
    pub fn from_env() -> Self {
        let budget = match std::env::var("CASCADE_RELAY_DAILY_BYTES") {
            Ok(raw) if !raw.is_empty() => raw.parse().unwrap_or(DEFAULT_RELAY_DAILY_BYTES),
            _ => DEFAULT_RELAY_DAILY_BYTES,
        };
        Self::new(budget)
    }

    /// Account `bytes` against `owner`'s UTC-day budget.
    ///
    /// Allocates only when inserting a new owner entry.
    pub fn allow(&mut self, owner: &str, bytes: usize) -> BandwidthDecision {
        let today = Utc::now().date_naive();
        let n = bytes as u64;
        if let Some((used, day, warned)) = self.accounts.get_mut(owner) {
            if *day != today {
                *used = 0;
                *day = today;
                *warned = false;
            }
            let next = used.saturating_add(n);
            *used = next;
            if next <= self.budget {
                BandwidthDecision::Allow
            } else if !*warned {
                *warned = true;
                BandwidthDecision::Warn
            } else {
                BandwidthDecision::Drop
            }
        } else {
            let over = n > self.budget;
            self.accounts.insert(owner.to_string(), (n, today, over));
            if over {
                BandwidthDecision::Warn
            } else {
                BandwidthDecision::Allow
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn under_budget_allows() {
        let mut ledger = BandwidthLedger::new(10);
        assert_eq!(ledger.allow("a", 4), BandwidthDecision::Allow);
        assert_eq!(ledger.allow("a", 6), BandwidthDecision::Allow);
        assert_eq!(ledger.allow("b", 10), BandwidthDecision::Allow);
    }

    #[test]
    fn first_overage_warns_once() {
        let mut ledger = BandwidthLedger::new(10);
        assert_eq!(ledger.allow("a", 8), BandwidthDecision::Allow);
        assert_eq!(ledger.allow("a", 3), BandwidthDecision::Warn);
    }

    #[test]
    fn later_overages_drop() {
        let mut ledger = BandwidthLedger::new(10);
        assert_eq!(ledger.allow("a", 11), BandwidthDecision::Warn);
        assert_eq!(ledger.allow("a", 1), BandwidthDecision::Drop);
        assert_eq!(ledger.allow("a", 5), BandwidthDecision::Drop);
    }

    #[test]
    fn day_rollover_resets() {
        let mut ledger = BandwidthLedger::new(10);
        assert_eq!(ledger.allow("a", 11), BandwidthDecision::Warn);
        assert_eq!(ledger.allow("a", 1), BandwidthDecision::Drop);
        let yesterday = Utc::now().date_naive() - Duration::days(1);
        if let Some((_, day, _)) = ledger.accounts.get_mut("a") {
            *day = yesterday;
        }
        assert_eq!(ledger.allow("a", 4), BandwidthDecision::Allow);
        assert_eq!(ledger.allow("a", 7), BandwidthDecision::Warn);
    }
}
