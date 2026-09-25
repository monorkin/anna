//! What Anna has going right now, for whoever asks from the terminal.
//!
//! A count of busy conversations says she is doing something but not what,
//! and the things worth knowing are spread out: the board holds what a
//! thread claimed, the dispatcher holds what is queued behind a turn, and
//! only the turn itself knows it started a hand and when. None of that
//! survives the turn, so it is written down here while it happens and read
//! back whole by `anna status`.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct AtWork {
    turns: Mutex<HashMap<String, Turn>>,
}

pub struct Turn {
    pub began: Instant,
    pub hands: Vec<Hand>,
}

pub struct Hand {
    pub id: String,
    pub project: String,
    pub began: Instant,
}

pub struct Going {
    pub conversation: String,
    pub taken: Duration,
    pub hands: Vec<HandAt>,
}

pub struct HandAt {
    pub hand: String,
    pub project: String,
    pub taken: Duration,
}

/// A turn is taken off the picture however it ends — a reply, an error, a
/// panic — so a crashed turn doesn't sit in `anna status` for ever.
pub struct WhileItRuns {
    at_work: Arc<AtWork>,
    conversation: String,
}

impl AtWork {
    pub fn turn_began(self: &Arc<AtWork>, conversation: &str) -> WhileItRuns {
        self.turns
            .lock()
            .unwrap()
            .insert(conversation.to_string(), Turn { began: Instant::now(), hands: Vec::new() });
        WhileItRuns { at_work: self.clone(), conversation: conversation.to_string() }
    }

    pub fn hand_began(&self, conversation: &str, id: &str, project: &str) {
        if let Some(turn) = self.turns.lock().unwrap().get_mut(conversation) {
            turn.hands.push(Hand { id: id.to_string(), project: project.to_string(), began: Instant::now() });
        }
    }

    pub fn hand_ended(&self, conversation: &str, id: &str) {
        if let Some(turn) = self.turns.lock().unwrap().get_mut(conversation) {
            turn.hands.retain(|hand| hand.id != id);
        }
    }

    /// Every turn going, longest first, with the clock already read so the
    /// caller doesn't hold the lock to do it.
    pub fn going_on(&self) -> Vec<Going> {
        let now = Instant::now();
        let turns = self.turns.lock().unwrap();
        let mut going: Vec<Going> = turns
            .iter()
            .map(|(conversation, turn)| Going {
                conversation: conversation.clone(),
                taken: now - turn.began,
                hands: turn
                    .hands
                    .iter()
                    .map(|hand| HandAt {
                        hand: hand.id.clone(),
                        project: hand.project.clone(),
                        taken: now - hand.began,
                    })
                    .collect(),
            })
            .collect();
        going.sort_by_key(|going| Reverse(going.taken));
        going
    }
}

impl Drop for WhileItRuns {
    fn drop(&mut self) {
        self.at_work.turns.lock().unwrap().remove(&self.conversation);
    }
}

/// `38m`, `2h 5m`, `12s` — long enough to see at a glance whether something
/// is stuck.
pub fn how_long(taken: Duration) -> String {
    let seconds = taken.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_and_its_hands_are_there_while_they_run_and_gone_after() {
        let at_work = Arc::new(AtWork::default());
        assert!(at_work.going_on().is_empty());

        let running = at_work.turn_began("basecamp-one");
        at_work.hand_began("basecamp-one", "h1", "/home/x/hey-ng");
        at_work.hand_began("basecamp-one", "h2", "/home/x/haystack");
        at_work.hand_ended("basecamp-one", "h1");

        let going = at_work.going_on();
        assert_eq!(going.len(), 1);
        assert_eq!(going[0].conversation, "basecamp-one");
        assert_eq!(going[0].hands.len(), 1, "the hand that ended is gone, the other one is still there");
        assert_eq!(going[0].hands[0].project, "/home/x/haystack");

        at_work.hand_began("basecamp-two", "h3", "/home/x/anna");
        assert_eq!(at_work.going_on().len(), 1, "a hand of a turn nobody is running is not kept");

        drop(running);
        assert!(at_work.going_on().is_empty(), "the turn takes its hands with it however it ended");
    }

    #[test]
    fn how_long_reads_at_a_glance() {
        assert_eq!(how_long(Duration::from_secs(12)), "12s");
        assert_eq!(how_long(Duration::from_secs(60)), "1m");
        assert_eq!(how_long(Duration::from_secs(38 * 60 + 40)), "38m");
        assert_eq!(how_long(Duration::from_secs(2 * 3600 + 5 * 60)), "2h 5m");
    }
}
