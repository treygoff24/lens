use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::providers::Spend;

#[derive(Debug)]
pub struct Budget {
    max_dollars: Option<f64>,
    max_seconds: Option<u64>,
    start: Instant,
    hit: OnceLock<&'static str>,
    outstanding_dollars: Mutex<f64>,
}

impl Budget {
    pub fn new(max_dollars: Option<f64>, max_seconds: Option<u64>) -> Self {
        Self {
            max_dollars,
            max_seconds,
            start: Instant::now(),
            hit: OnceLock::new(),
            outstanding_dollars: Mutex::new(0.0),
        }
    }

    /// Records a pre-launch reservation if actual spend plus outstanding
    /// reservations plus this projection fits within the caps.
    ///
    /// Dropping the returned guard releases the reservation, so panicked
    /// workers cannot leak budget. Actual spend is recorded by the shared
    /// `Spend` meter.
    pub fn reserve(&self, spend: &Spend, projected_unit_cost: f64) -> Option<Reservation<'_>> {
        if self.hit.get().is_some() {
            return None;
        }

        let mut outstanding = self.outstanding();
        if !self.fits(spend, projected_unit_cost, *outstanding) {
            return None;
        }

        *outstanding += projected_unit_cost;
        Some(Reservation {
            budget: self,
            dollars: projected_unit_cost,
        })
    }

    pub fn hit(&self) -> Option<&str> {
        self.hit.get().copied()
    }

    fn fits(&self, spend: &Spend, projected_unit_cost: f64, outstanding: f64) -> bool {
        if self.max_dollars.is_some_and(|cap| {
            spend.total_dollars() + outstanding + projected_unit_cost > cap + 1e-9
        }) {
            self.hit.get_or_init(|| "dollars");
            return false;
        }

        if self
            .max_seconds
            .is_some_and(|cap| self.start.elapsed() > Duration::from_secs(cap))
        {
            self.hit.get_or_init(|| "seconds");
            return false;
        }

        true
    }

    fn outstanding(&self) -> MutexGuard<'_, f64> {
        self.outstanding_dollars.lock().unwrap()
    }

    fn release(&self, dollars: f64) {
        let mut outstanding = self.outstanding();
        *outstanding = (*outstanding - dollars).max(0.0);
    }
}

#[derive(Debug)]
pub struct Reservation<'a> {
    budget: &'a Budget,
    dollars: f64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.budget.release(self.dollars);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_caps_always_allows_launch() {
        let budget = Budget::new(None, None);

        assert!(budget.reserve(&Spend::default(), f64::MAX).is_some());
        assert_eq!(budget.hit(), None);
    }

    #[test]
    fn dollar_cap_blocks_projected_overspend_and_sticks() {
        let budget = Budget::new(Some(0.10), None);
        let spend = Spend {
            dollars: 0.09,
            ..Spend::default()
        };

        assert!(budget.reserve(&spend, 0.02).is_none());
        assert_eq!(budget.hit(), Some("dollars"));
        assert!(budget.reserve(&Spend::default(), 0.0).is_none());
        assert_eq!(budget.hit(), Some("dollars"));
    }

    #[test]
    fn reservations_count_outstanding_until_drop() {
        let budget = Budget::new(Some(0.10), None);
        let spend = Spend::default();
        let _first = budget.reserve(&spend, 0.05).unwrap();
        let _second = budget.reserve(&spend, 0.05).unwrap();

        assert!(budget.reserve(&spend, 0.05).is_none());
        assert_eq!(budget.hit(), Some("dollars"));
    }

    #[test]
    fn dropping_reservation_releases_projected_cost() {
        let budget = Budget::new(Some(0.10), None);
        let spend = Spend::default();

        drop(budget.reserve(&spend, 0.06).unwrap());

        assert!(budget.reserve(&spend, 0.10).is_some());
        assert_eq!(budget.hit(), None);
    }

    #[test]
    fn reservation_hit_reason_sticks_after_release() {
        let budget = Budget::new(Some(0.10), None);
        let first = budget.reserve(&Spend::default(), 0.07).unwrap();

        assert!(budget.reserve(&Spend::default(), 0.04).is_none());
        assert_eq!(budget.hit(), Some("dollars"));

        drop(first);
        assert!(budget.reserve(&Spend::default(), 0.0).is_none());
        assert_eq!(budget.hit(), Some("dollars"));
    }

    #[test]
    fn seconds_cap_blocks_after_elapsed_time() {
        let budget = Budget::new(None, Some(0));
        std::thread::sleep(Duration::from_millis(2));

        assert!(budget.reserve(&Spend::default(), 0.0).is_none());
        assert_eq!(budget.hit(), Some("seconds"));
    }

    #[test]
    fn f4_epsilon_boundary_allows_exact_fit() {
        // Binary floating point puts 0.001 + 0.008 just above the exact cap.
        let budget = Budget::new(Some(0.009), None);
        let spend = Spend {
            dollars: 0.001,
            ..Spend::default()
        };

        assert!(budget.reserve(&spend, 0.008).is_some());
        assert_eq!(budget.hit(), None);
    }
}
