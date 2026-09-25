//! What is waiting to be said in each conversation that has a turn going.
//!
//! A conversation's turns run one at a time and in the order they came: a
//! "never mind" must not overtake what it takes back. One worker drains a
//! conversation's queue and leaves when it is empty, so there are as many
//! threads as there are busy conversations, not as many as there are
//! messages.

use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread as os_thread;

use crate::logs;

const MOST_TURNS_WAITING: usize = 50;

#[derive(Default)]
pub struct Turns {
    waiting: Mutex<HashMap<String, VecDeque<Turn>>>,
}

pub type Turn = Box<dyn FnOnce() + Send>;

impl Turns {
    pub fn add(self: &Arc<Turns>, conversation: &str, turn: Turn) {
        let mut waiting = self.waiting.lock().unwrap();
        match waiting.get_mut(conversation) {
            Some(queue) if queue.len() >= MOST_TURNS_WAITING => {
                logs::event("turn.dropped", json!({ "conversation": conversation, "waiting": queue.len() }));
            }
            Some(queue) => queue.push_back(turn),
            None => {
                waiting.insert(conversation.to_string(), VecDeque::new());
                let turns = self.clone();
                let conversation = conversation.to_string();
                os_thread::spawn(move || turns.work_through(&conversation, turn));
            }
        }
    }

    fn work_through(&self, conversation: &str, first: Turn) {
        let mut turn = first;
        loop {
            turn();
            let mut waiting = self.waiting.lock().unwrap();
            match waiting.get_mut(conversation).and_then(|queue| queue.pop_front()) {
                Some(next) => turn = next,
                None => {
                    waiting.remove(conversation);
                    return;
                }
            }
        }
    }

    pub fn busy_conversations(&self) -> usize {
        self.waiting.lock().unwrap().len()
    }

    /// How much is stacked up behind each conversation's running turn. A
    /// thread deep in a hand can't hear anything, so this is where a message
    /// that looks ignored actually is.
    pub fn waiting_behind(&self) -> HashMap<String, usize> {
        self.waiting.lock().unwrap().iter().map(|(conversation, queue)| (conversation.clone(), queue.len())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn until(it_is_so: impl Fn() -> bool) {
        for _ in 0..500 {
            if it_is_so() {
                return;
            }
            os_thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_conversations_turns_run_in_the_order_they_came_while_others_go_on() {
        let turns = Arc::new(Turns::default());
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (first_may_finish, waiting_to_finish) = mpsc::channel::<()>();

        let record = ran.clone();
        turns.add("card-1", Box::new(move || {
            let _ = waiting_to_finish.recv();
            record.lock().unwrap().push("card-1: do it".to_string());
        }));
        for said in ["card-1: never mind", "card-1: actually, do"] {
            let record = ran.clone();
            turns.add("card-1", Box::new(move || record.lock().unwrap().push(said.to_string())));
        }

        let (elsewhere_ran, elsewhere) = mpsc::channel();
        turns.add("card-2", Box::new(move || elsewhere_ran.send(()).unwrap()));
        elsewhere.recv_timeout(Duration::from_secs(5)).expect("another conversation waited on a busy one");
        until(|| turns.busy_conversations() == 1);
        assert_eq!(turns.busy_conversations(), 1, "card-2 is done and gone, card-1 is still held up");

        first_may_finish.send(()).unwrap();
        until(|| turns.busy_conversations() == 0);
        assert_eq!(*ran.lock().unwrap(), ["card-1: do it", "card-1: never mind", "card-1: actually, do"]);
        assert_eq!(turns.busy_conversations(), 0);
    }
}
