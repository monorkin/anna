//! A circuit breaker for a service Anna can do without.
//!
//! Jev makes her judgments fast, but haiku can make them too, so a Jev that
//! is struggling should be left alone rather than waited on: every call that
//! fails or takes too long costs a message its answer time. Enough of those
//! in a short while and the breaker opens — nothing is sent for half an hour,
//! then it is tried again. A refused key is different. Nothing will fix that
//! but a person, so it stays off until she is restarted.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const FAILURES_THAT_OPEN_IT: usize = 5;
const WITHIN: Duration = Duration::from_secs(10 * 60);
const STAYS_OPEN_FOR: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Answered,
    /// An error, or an answer that took too long to be worth having.
    Failed,
    /// The key was refused.
    Unauthorized,
}

/// What recording an outcome changed, for whoever keeps the log.
#[derive(Debug, PartialEq)]
pub enum Change {
    Nothing,
    Opened { minutes: u64 },
    SwitchedOff,
}

#[derive(Default)]
pub struct Breaker {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    failures: VecDeque<Instant>,
    open_until: Option<Instant>,
    switched_off: bool,
}

impl Breaker {
    pub fn allows(&self, now: Instant) -> bool {
        let state = self.state.lock().unwrap();
        !state.switched_off && state.open_until.is_none_or(|until| now >= until)
    }

    pub fn record(&self, outcome: Outcome, now: Instant) -> Change {
        let mut state = self.state.lock().unwrap();
        match outcome {
            Outcome::Answered => {
                state.open_until = None;
                Change::Nothing
            }
            Outcome::Unauthorized => {
                state.switched_off = true;
                Change::SwitchedOff
            }
            Outcome::Failed => {
                state.failures.push_back(now);
                while state.failures.front().is_some_and(|it| now.duration_since(*it) > WITHIN) {
                    state.failures.pop_front();
                }

                if state.failures.len() >= FAILURES_THAT_OPEN_IT {
                    state.failures.clear();
                    state.open_until = Some(now + STAYS_OPEN_FOR);
                    Change::Opened { minutes: STAYS_OPEN_FOR.as_secs() / 60 }
                } else {
                    Change::Nothing
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minutes(count: u64) -> Duration {
        Duration::from_secs(count * 60)
    }

    #[test]
    fn enough_failures_in_a_short_while_open_it_for_half_an_hour() {
        let breaker = Breaker::default();
        let start = Instant::now();

        for _ in 0..4 {
            assert_eq!(breaker.record(Outcome::Failed, start), Change::Nothing);
        }
        assert!(breaker.allows(start));
        assert_eq!(breaker.record(Outcome::Failed, start + minutes(1)), Change::Opened { minutes: 30 });

        assert!(!breaker.allows(start + minutes(2)));
        assert!(!breaker.allows(start + minutes(30)));
        assert!(breaker.allows(start + minutes(31)));
    }

    #[test]
    fn failures_spread_over_a_long_while_do_not_open_it() {
        let breaker = Breaker::default();
        let start = Instant::now();

        for index in 0..12 {
            assert_eq!(breaker.record(Outcome::Failed, start + minutes(index * 3)), Change::Nothing);
        }
        assert!(breaker.allows(start + minutes(40)));
    }

    #[test]
    fn a_refused_key_switches_it_off_for_good() {
        let breaker = Breaker::default();
        let start = Instant::now();

        assert_eq!(breaker.record(Outcome::Unauthorized, start), Change::SwitchedOff);
        assert!(!breaker.allows(start));
        assert!(!breaker.allows(start + minutes(600)));

        breaker.record(Outcome::Answered, start + minutes(601));
        assert!(!breaker.allows(start + minutes(602)));
    }
}
