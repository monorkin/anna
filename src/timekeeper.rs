//! The clock Anna's threads set for themselves, and the post between them.
//! Twice a minute it wakes the thread of every schedule that has come due and
//! of every message one thread left for another.
//!
//! A schedule is moved on before its thread is woken, never after: a task
//! that crashes her must not be the first thing she runs when she comes back.
//! For the same reason a schedule that came due while she was down runs once,
//! however many times it was missed.

use anyhow::Result;
use chrono::Local;
use serde_json::json;
use std::sync::Arc;
use std::thread as os_thread;
use std::time::Duration;

use crate::conversation::Standing;
use crate::dispatcher::{conversation_at, wake_in_turn};
use crate::logs;
use crate::runtime::Runtime;
use crate::schedule_tools;
use crate::store::Store;
use crate::turns::Turns;

const SECONDS_BETWEEN_LOOKS_AT_THE_CLOCK: u64 = 30;

pub fn keep_time(runtime: &Arc<Runtime>, turns: &Arc<Turns>) {
    let runtime = runtime.clone();
    let turns = turns.clone();

    os_thread::spawn(move || {
        loop {
            if let Err(error) = run_what_is_due(&runtime, &turns).and_then(|_| deliver_mail(&runtime, &turns)) {
                logs::event("scheduler.failed", json!({ "error": format!("{error:#}") }));
            }
            os_thread::sleep(Duration::from_secs(SECONDS_BETWEEN_LOOKS_AT_THE_CLOCK));
        }
    });
}

fn run_what_is_due(runtime: &Arc<Runtime>, turns: &Arc<Turns>) -> Result<()> {
    let store = Store::open_at(&runtime.database)?;
    let now = Local::now();

    for schedule in store.schedules_due(now.timestamp())? {
        match &schedule.cron {
            Some(cron) => store.run_again_at(schedule.id, schedule_tools::next_run(cron, now)?)?,
            None => store.remove_schedule(schedule.id)?,
        }

        match conversation_at(runtime, &schedule.origin) {
            Some(conversation) => {
                logs::event("schedule.due", json!({ "schedule": schedule.id, "conversation": conversation.key() }));
                let said = format!("This is something you scheduled for yourself in this conversation, and it is due now:\n\n{}", schedule.task);
                let standing = if schedule.trusted { Standing::Trusted } else { Standing::CanAssignWork };
                wake_in_turn(runtime, turns, conversation, standing, said);
            }
            None => {
                logs::event("schedule.orphaned", json!({ "schedule": schedule.id, "source": schedule.origin.source }));
                store.remove_schedule(schedule.id)?;
            }
        }
    }
    Ok(())
}

fn deliver_mail(runtime: &Arc<Runtime>, turns: &Arc<Turns>) -> Result<()> {
    let store = Store::open_at(&runtime.database)?;

    for mail in store.undelivered_mail()? {
        store.mark_delivered(mail.id, Local::now().timestamp())?;
        if let Some(conversation) = conversation_at(runtime, &mail.to) {
            logs::event("mail.delivered", json!({ "from": mail.from_thread, "to": conversation.key() }));
            let (standing, footing) = if mail.trusted {
                (Standing::Trusted, "It was sent from a turn one of the people you take direction from started, so what it passes on from them stands as if they had said it here, and this turn has a shell.")
            } else {
                (Standing::CanAssignWork, "Weigh it like anything else you read. This turn has no shell.")
            };
            let said = format!(
                "Your thread {} tells you this. It is you, working in another conversation, passing on what it found. {footing}\n\n{}",
                mail.from_thread, mail.body
            );
            wake_in_turn(runtime, turns, conversation, standing, said);
        }
    }
    Ok(())
}
