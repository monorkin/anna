//! Which of her Claude accounts she works as. Threads run as the default
//! Claude login and every hand copies its token from there, so moving the
//! default login to the account with the most room covers both — for what
//! starts next. What is already running on the old login is moved by its
//! caller, which knows what it was in the middle of.

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::logs;

const SWITCH_AT_PERCENT: f64 = 90.0;
/// ax answers from its last reading when the endpoint was asked in the past
/// ten minutes, so asking oftener than that only burns wake-ups.
const SECONDS_BETWEEN_CHECKS: u64 = 600;
const SECONDS_A_SWITCH_ANSWERS_FOR: u64 = 300;

static ROTATING: AtomicBool = AtomicBool::new(false);

/// ax's auto-switch for as long as Anna runs. Only what changes is logged: a
/// switch, or a check that failed.
pub fn rotate() {
    ROTATING.store(true, Ordering::Relaxed);
    thread::spawn(|| {
        let mut last = String::new();
        loop {
            let outcome = match ax::auto_switch::tick(SWITCH_AT_PERCENT) {
                Ok(outcome) => outcome,
                Err(error) => format!("check failed: {error:#}"),
            };
            if outcome != last && !outcome.contains("staying put") {
                logs::event("accounts.checked", json!({ "outcome": outcome }));
            }
            last = outcome;
            thread::sleep(Duration::from_secs(SECONDS_BETWEEN_CHECKS));
        }
    });
}

/// Claude refusing a run for the limit is the surest reading there is.
/// Rotation goes by the usage endpoint, whose kept reading said 86% of an
/// account Claude had already stopped, and every thread sat waiting hours
/// for a reset with another account barely touched. So a refusal moves her
/// to the stored account with the most room, when one has any and she
/// rotates at all. Runs hit the limit together: one switch answers all of
/// them for a while, or the next would switch back to the spent account on
/// its stale reading.
pub fn moved_off_the_spent_account() -> bool {
    static LAST_SWITCH: Mutex<Option<Instant>> = Mutex::new(None);
    if !ROTATING.load(Ordering::Relaxed) {
        return false;
    }
    let mut last = LAST_SWITCH.lock().unwrap();
    if last.is_some_and(|it| it.elapsed() < Duration::from_secs(SECONDS_A_SWITCH_ANSWERS_FOR)) {
        return true;
    }

    let Ok(reports) = ax::usage::of_every_account() else {
        return false;
    };
    let roomiest = reports
        .iter()
        .filter(|it| !it.active)
        .filter_map(|it| Some((it, it.reading.as_ref()?.usage.utilization()?)))
        .filter(|(_, used)| *used < SWITCH_AT_PERCENT)
        .min_by(|(_, one), (_, other)| one.total_cmp(other));
    match roomiest {
        Some((report, used)) if ax::account::switch(&report.account.email).is_ok() => {
            logs::event("accounts.switched_at_the_limit", json!({ "to": report.account.email, "used": used }));
            *last = Some(Instant::now());
            true
        }
        _ => false,
    }
}
