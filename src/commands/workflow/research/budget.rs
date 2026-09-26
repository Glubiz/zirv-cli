//! Budget reservation and accounting: reserve before dispatch, charge on
//! completion. A trial's ceiling is reserved the instant it is scheduled
//! (before any external execution) so concurrent in-flight work is always
//! counted against the cap, never just the settled total -- issue #802's
//! "reserve budget for in-flight work" requirement.

use super::manifest::Budgets;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetExhausted {
    Spend,
    Calls,
    Trials,
    Wall,
}

impl BudgetExhausted {
    pub fn reason(self) -> &'static str {
        match self {
            BudgetExhausted::Spend => "budget_exhausted:spend",
            BudgetExhausted::Calls => "budget_exhausted:calls",
            BudgetExhausted::Trials => "budget_exhausted:trials",
            BudgetExhausted::Wall => "budget_exhausted:wall",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Tracker {
    pub spent_usd: f64,
    pub reserved_usd: f64,
    pub calls_used: u64,
    pub reserved_calls: u64,
    pub trials_dispatched: u64,
    pub campaign_started_at: u64,
}

impl Tracker {
    pub fn new(campaign_started_at: u64) -> Self {
        Self {
            campaign_started_at,
            ..Default::default()
        }
    }

    /// Whether reserving `ceiling_usd`/`calls`, plus one more trial, plus a
    /// dispatch that will not finish before `timeout_secs` from `now`,
    /// would exceed any cap. Checked BEFORE every dispatch, per the design.
    pub fn check_reservation(
        &self,
        budgets: &Budgets,
        ceiling_usd: f64,
        calls: u64,
        timeout_secs: u64,
        now: u64,
    ) -> Result<(), BudgetExhausted> {
        if self.spent_usd + self.reserved_usd + ceiling_usd > budgets.max_spend_usd {
            return Err(BudgetExhausted::Spend);
        }
        if self.calls_used + self.reserved_calls + calls > budgets.max_calls {
            return Err(BudgetExhausted::Calls);
        }
        if self.trials_dispatched + 1 > budgets.max_trials {
            return Err(BudgetExhausted::Trials);
        }
        let elapsed = now.saturating_sub(self.campaign_started_at);
        if elapsed + timeout_secs > budgets.max_wall_secs {
            return Err(BudgetExhausted::Wall);
        }
        Ok(())
    }

    pub fn reserve(&mut self, ceiling_usd: f64, calls: u64) {
        self.reserved_usd += ceiling_usd;
        self.reserved_calls += calls;
        self.trials_dispatched += 1;
    }

    /// Releases a trial's reservation and charges the actuals -- when
    /// completeness was not `complete`, callers pass
    /// `actual_usd = ceiling_usd.max(known_usd)` themselves (the "unknown
    /// final spend is charged at the ceiling" rule).
    pub fn settle(
        &mut self,
        ceiling_usd: f64,
        reserved_calls: u64,
        actual_usd: f64,
        actual_calls: u64,
    ) {
        self.reserved_usd = (self.reserved_usd - ceiling_usd).max(0.0);
        self.reserved_calls = self.reserved_calls.saturating_sub(reserved_calls);
        self.spent_usd += actual_usd;
        self.calls_used += actual_calls;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budgets() -> Budgets {
        Budgets {
            max_spend_usd: 10.0,
            max_wall_secs: 1000,
            max_calls: 20,
            max_trials: 5,
            max_retries: 1,
            concurrency: 2,
        }
    }

    #[test]
    fn a_reservation_that_would_exceed_spend_is_refused() {
        let mut tracker = Tracker::new(0);
        tracker.reserve(9.5, 1);
        let err = tracker
            .check_reservation(&budgets(), 1.0, 1, 10, 0)
            .expect_err("must exceed max_spend_usd");
        assert_eq!(err, BudgetExhausted::Spend);
    }

    #[test]
    fn in_flight_reservations_count_toward_the_cap() {
        let mut tracker = Tracker::new(0);
        // Two trials at 5.0 each reserved but not yet settled == 10.0,
        // exactly the cap: a third must be refused even though nothing has
        // "spent" yet.
        tracker.reserve(5.0, 1);
        tracker.reserve(5.0, 1);
        let err = tracker
            .check_reservation(&budgets(), 0.01, 1, 10, 0)
            .expect_err("in-flight reservations must count");
        assert_eq!(err, BudgetExhausted::Spend);
    }

    #[test]
    fn settling_releases_the_reservation_and_charges_the_actual() {
        let mut tracker = Tracker::new(0);
        tracker.reserve(5.0, 4);
        tracker.settle(5.0, 4, 2.0, 3);
        assert_eq!(tracker.reserved_usd, 0.0);
        assert_eq!(tracker.reserved_calls, 0);
        assert_eq!(tracker.spent_usd, 2.0);
        assert_eq!(tracker.calls_used, 3);
        assert!(tracker.check_reservation(&budgets(), 7.0, 1, 10, 0).is_ok());
    }

    #[test]
    fn wall_budget_refuses_a_dispatch_that_cannot_finish_in_time() {
        let tracker = Tracker::new(100);
        let err = tracker
            .check_reservation(&budgets(), 0.0, 0, 950, 200)
            .expect_err("elapsed + timeout must exceed max_wall_secs");
        assert_eq!(err, BudgetExhausted::Wall);
    }
}
