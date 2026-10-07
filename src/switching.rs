//! Moving to another Claude account without carrying full sessions over.
//!
//! A prompt cache belongs to the account that wrote it, so after a switch
//! every thread's next turn writes its whole session again, on the new
//! account. A planned switch therefore winds down first, on the old account
//! while it still has room: new turns wait, the turns running are let
//! finish, each hand at work is stopped and asked for a short account of
//! where it stands, and every thread is compacted. Then the switch, and what
//! waited goes on — the hands' accounts first among it.
//!
//! A switch because Claude refused a run for the limit can't wind down: the
//! old account has nothing left to do it with.

use serde_json::json;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cooling;
use crate::logs;
use crate::runtime::Runtime;

const LONGEST_WAIT_FOR_TURNS: Duration = Duration::from_secs(15 * 60);
const LONGEST_WAIT_FOR_HANDS: Duration = Duration::from_secs(10 * 60);

static WINDING_DOWN: Mutex<bool> = Mutex::new(false);
static WOUND_UP: Condvar = Condvar::new();

/// Everything up to the switch itself, which `switch` carries out once this
/// is done. Turns that come meanwhile wait, and go on when it returns.
pub fn wind_down_then<T>(runtime: &Arc<Runtime>, switch: impl FnOnce() -> T) -> T {
    *WINDING_DOWN.lock().unwrap() = true;
    logs::event("accounts.winding_down", json!({}));

    wait_for(LONGEST_WAIT_FOR_TURNS, || runtime.cooling.running().is_empty());
    let stopped = runtime.hands.stop_all_for_a_switch();
    wait_for(LONGEST_WAIT_FOR_HANDS, || !runtime.hands.are_working());
    let compacted = cooling::compact_every_thread();
    logs::event("accounts.wound_down", json!({ "hands_stopped": stopped, "threads_compacted": compacted }));

    let switched = switch();
    *WINDING_DOWN.lock().unwrap() = false;
    WOUND_UP.notify_all();
    switched
}

/// Where a turn waits while she winds down, so nothing new starts on the
/// account she is leaving.
pub fn wait_while_winding_down() {
    let mut winding_down = WINDING_DOWN.lock().unwrap();
    while *winding_down {
        winding_down = WOUND_UP.wait(winding_down).unwrap();
    }
}

fn wait_for(longest: Duration, done: impl Fn() -> bool) {
    let since = Instant::now();
    while !done() && since.elapsed() < longest {
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_turn_waits_while_she_winds_down_and_goes_on_after() {
        *WINDING_DOWN.lock().unwrap() = true;
        let went_on = Arc::new(AtomicBool::new(false));
        let turn = {
            let went_on = went_on.clone();
            std::thread::spawn(move || {
                wait_while_winding_down();
                went_on.store(true, Ordering::Relaxed);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!went_on.load(Ordering::Relaxed), "it waits");

        *WINDING_DOWN.lock().unwrap() = false;
        WOUND_UP.notify_all();
        turn.join().unwrap();
        assert!(went_on.load(Ordering::Relaxed), "and goes on once the switch is done");
    }
}
