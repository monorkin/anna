//! What is said to a conversation while its thread is busy, read together.
//!
//! Every turn resends the whole session, so seven comments read one turn
//! each cost seven times what they cost read in one. Each message is posted
//! here as it comes, and a turn, when its time comes, takes every message at
//! the front that runs on the same standing — in the order they came, so a
//! "never mind" is read after what it takes back, and nothing is read on a
//! standing it didn't come with. The turns queued for messages already taken
//! find nothing, and end.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::conversation::Standing;

const BETWEEN_MESSAGES: &str = "\n\n---\n\nAnd after that, before you could answer:\n\n";

#[derive(Default)]
pub struct Mailbox {
    waiting: Mutex<HashMap<String, VecDeque<Letter>>>,
}

struct Letter {
    standing: Standing,
    said: String,
    kept: Option<i64>,
}

/// What a turn reads: one message or several, and the kept turns it answers.
#[derive(Debug, PartialEq)]
pub struct Post {
    pub standing: Standing,
    pub said: String,
    pub kept: Vec<i64>,
}

impl Mailbox {
    pub fn post(&self, conversation: &str, standing: Standing, said: String, kept: Option<i64>) {
        self.waiting.lock().unwrap().entry(conversation.to_string()).or_default().push_back(Letter { standing, said, kept });
    }

    pub fn take(&self, conversation: &str) -> Option<Post> {
        let mut waiting = self.waiting.lock().unwrap();
        let letters = waiting.get_mut(conversation)?;
        let standing = letters.front()?.standing;
        let mut taken = Vec::new();
        while letters.front().is_some_and(|it| it.standing == standing) {
            taken.extend(letters.pop_front());
        }
        if letters.is_empty() {
            waiting.remove(conversation);
        }

        Some(Post {
            standing,
            said: taken.iter().map(|it| it.said.as_str()).collect::<Vec<_>>().join(BETWEEN_MESSAGES),
            kept: taken.iter().filter_map(|it| it.kept).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_came_on_one_standing_is_read_in_one_turn_and_in_order() {
        let mailbox = Mailbox::default();
        mailbox.post("card-1", Standing::CanAssignWork, "Marko: the build is red".to_string(), Some(1));
        mailbox.post("card-1", Standing::CanAssignWork, "Marko: never mind, it's green".to_string(), Some(2));
        mailbox.post("card-1", Standing::Trusted, "Marta: ship it".to_string(), None);
        mailbox.post("card-1", Standing::CanAssignWork, "Marko: thanks".to_string(), Some(4));
        mailbox.post("card-2", Standing::Trusted, "Marta: look at the blog".to_string(), Some(5));

        assert_eq!(
            mailbox.take("card-1"),
            Some(Post {
                standing: Standing::CanAssignWork,
                said: format!("Marko: the build is red{BETWEEN_MESSAGES}Marko: never mind, it's green"),
                kept: vec![1, 2],
            })
        );
        assert_eq!(
            mailbox.take("card-1"),
            Some(Post { standing: Standing::Trusted, said: "Marta: ship it".to_string(), kept: vec![] }),
            "a trusted word isn't read on an untrusted one's standing, nor ahead of it"
        );
        assert_eq!(mailbox.take("card-1").unwrap().kept, [4]);
        assert_eq!(mailbox.take("card-1"), None, "the turns queued for what was already read find nothing");
        assert_eq!(mailbox.take("card-2").unwrap().said, "Marta: look at the blog");
    }
}
