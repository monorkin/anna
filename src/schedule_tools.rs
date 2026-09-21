//! How a thread sets up work for its future self: "every weekday at nine,
//! check the deploy queue", "on Friday, remind Marta about the invoice".
//!
//! A schedule belongs to the conversation it was made in. When it comes due
//! the scheduler wakes that conversation's thread with the task, as if
//! someone had just asked for it there, and whatever the thread says lands in
//! that conversation.
//!
//! Recurring schedules are five-field cron expressions in the machine's own
//! time zone — the one format every model already knows. The limits are in
//! code because nobody is around to notice a thread that was talked into
//! waking itself every minute: no schedule may run more often than every five
//! minutes, and a conversation may hold twenty.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use croner::Cron;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::str::FromStr;

use crate::broker::Tool;
use crate::clock;
use crate::conversation::{Origin, Standing};
use crate::logs;
use crate::store::Store;

const SHORTEST_GAP_SECONDS: i64 = 300;
const MOST_PER_CONVERSATION: usize = 20;
const ACCEPTED_DATES: [&str; 3] = ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"];

/// When a cron expression next matches after a moment, in unix seconds.
pub fn next_run(cron: &str, after: DateTime<Local>) -> Result<i64> {
    let parsed = Cron::from_str(cron).with_context(|| format!("\"{cron}\" is not a five-field cron expression"))?;
    let next = parsed
        .find_next_occurrence(&after, false)
        .with_context(|| format!("\"{cron}\" never comes due"))?;
    Ok(next.timestamp())
}

fn refuse_if_too_often(cron: &str, now: DateTime<Local>) -> Result<()> {
    let first = next_run(cron, now)?;
    let second = next_run(cron, Local.timestamp_opt(first, 0).single().context("that time does not exist")?)?;
    if second - first < SHORTEST_GAP_SECONDS {
        bail!("that would run every {} seconds; the most often anything may run is every five minutes", second - first);
    }
    Ok(())
}

fn moment(text: &str, now: DateTime<Local>) -> Result<i64> {
    let naive = ACCEPTED_DATES
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text.trim(), format).ok())
        .with_context(|| format!("\"{text}\" is not a date and time like 2026-09-25 09:00"))?;
    let at = Local
        .from_local_datetime(&naive)
        .earliest()
        .context("that time does not exist here (a clock change skips it)")?;
    if at <= now {
        bail!("{text} is already past; it is {} now", now.format("%Y-%m-%d %H:%M"));
    }
    Ok(at.timestamp())
}

fn shown(unix_seconds: i64) -> String {
    match Local.timestamp_opt(unix_seconds, 0).single() {
        Some(at) => at.format("%a %Y-%m-%d %H:%M").to_string(),
        None => unix_seconds.to_string(),
    }
}

pub struct Schedule {
    pub database: PathBuf,
    pub origin: Origin,
    pub standing: Standing,
}

impl Tool for Schedule {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description(&self) -> &str {
        "Set up something for you to do later in this conversation, once or on a repeating schedule. When it comes due you are woken here with the task, as if someone had just asked. Give either `cron` — a five-field cron expression in local time, such as \"0 9 * * 1-5\" for weekdays at nine — or `at`, a local date and time such as \"2026-09-25 09:00\", for something that happens once. Nothing may repeat more often than every five minutes. Write the task as a full instruction: your future self starts with this conversation's history but not with what you are thinking now."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" },
                "cron": { "type": "string", "description": "minute hour day-of-month month day-of-week" },
                "at": { "type": "string", "description": "YYYY-MM-DD HH:MM, local time" },
            },
            "required": ["task"],
        })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let task = arguments["task"].as_str().filter(|it| !it.trim().is_empty()).context("task is required")?;
        let store = Store::open_at(&self.database)?;
        if store.schedules_of(&self.origin)?.len() >= MOST_PER_CONVERSATION {
            bail!("this conversation already has {MOST_PER_CONVERSATION} schedules; cancel one first");
        }

        let now = Local::now();
        let (cron, first_run) = match (arguments["cron"].as_str(), arguments["at"].as_str()) {
            (Some(cron), None) => {
                refuse_if_too_often(cron, now)?;
                (Some(cron), next_run(cron, now)?)
            }
            (None, Some(at)) => (None, moment(at, now)?),
            _ => bail!("give either cron or at, not both and not neither"),
        };

        let trusted = self.standing == Standing::Trusted;
        let id = store.add_schedule(&self.origin, cron, task, first_run, trusted, &clock::timestamp())?;
        logs::event("schedule.added", json!({ "schedule": id, "cron": cron, "first_run": shown(first_run) }));
        Ok(format!("Scheduled as number {id}. It first runs {}.", shown(first_run)))
    }
}

pub struct ListSchedules {
    pub database: PathBuf,
    pub origin: Origin,
}

impl Tool for ListSchedules {
    fn name(&self) -> &str {
        "list_schedules"
    }

    fn description(&self) -> &str {
        "List what is scheduled in this conversation: each schedule's number, when it runs next, how it repeats, and the task."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn call(&self, _arguments: &Value) -> Result<String> {
        let schedules = Store::open_at(&self.database)?.schedules_of(&self.origin)?;
        if schedules.is_empty() {
            return Ok("Nothing is scheduled in this conversation.".to_string());
        }

        Ok(schedules
            .iter()
            .map(|it| {
                let repeats = match &it.cron {
                    Some(cron) => format!("repeats on \"{cron}\""),
                    None => "runs once".to_string(),
                };
                format!("{}. next {}, {repeats}: {}", it.id, shown(it.next_run), it.task)
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

pub struct CancelSchedule {
    pub database: PathBuf,
    pub origin: Origin,
}

impl Tool for CancelSchedule {
    fn name(&self) -> &str {
        "cancel_schedule"
    }

    fn description(&self) -> &str {
        "Cancel a schedule in this conversation, by the number list_schedules shows."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "number": { "type": "integer" } }, "required": ["number"] })
    }

    fn call(&self, arguments: &Value) -> Result<String> {
        let number = arguments["number"].as_i64().context("number is required")?;
        if Store::open_at(&self.database)?.cancel_schedule(number, &self.origin)? {
            logs::event("schedule.cancelled", json!({ "schedule": number }));
            Ok(format!("Cancelled number {number}."))
        } else {
            bail!("there is no schedule {number} in this conversation")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Local> {
        Local
            .from_local_datetime(&NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap())
            .earliest()
            .unwrap()
    }

    #[test]
    fn cron_expressions_come_due_in_local_time() {
        let monday_morning = at("2026-09-21 08:00");

        assert_eq!(next_run("0 9 * * 1-5", monday_morning).unwrap(), at("2026-09-21 09:00").timestamp());
        assert_eq!(next_run("0 9 * * 1-5", at("2026-09-25 09:00")).unwrap(), at("2026-09-28 09:00").timestamp());
        assert!(next_run("every morning", monday_morning).is_err());
    }

    #[test]
    fn nothing_may_run_more_often_than_every_five_minutes() {
        let now = at("2026-09-21 08:00");

        assert!(refuse_if_too_often("* * * * *", now).unwrap_err().to_string().contains("every five minutes"));
        assert!(refuse_if_too_often("*/2 * * * *", now).is_err());
        assert!(refuse_if_too_often("*/5 * * * *", now).is_ok());
        assert!(refuse_if_too_often("0 9 * * *", now).is_ok());
    }

    #[test]
    fn one_off_moments_are_local_and_in_the_future() {
        let now = at("2026-09-21 08:00");

        assert_eq!(moment("2026-09-25 09:00", now).unwrap(), at("2026-09-25 09:00").timestamp());
        assert_eq!(moment("2026-09-25T09:00", now).unwrap(), at("2026-09-25 09:00").timestamp());
        assert!(moment("2026-09-20 09:00", now).unwrap_err().to_string().contains("already past"));
        assert!(moment("next Friday", now).is_err());
    }

    #[test]
    fn a_conversation_schedules_lists_and_cancels_its_own() {
        let directory = std::env::temp_dir().join(format!("anna-schedule-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let database = directory.join("anna.db");
        let origin = Origin { source: "basecamp".to_string(), conversation: "card-1".to_string() };
        let elsewhere = Origin { source: "basecamp".to_string(), conversation: "card-2".to_string() };

        let schedule = Schedule { database: database.clone(), origin: origin.clone(), standing: Standing::CanAssignWork };
        assert!(schedule.call(&json!({ "task": "Check the queue", "cron": "0 9 * * 1-5" })).unwrap().contains("number 1"));
        assert!(schedule.call(&json!({ "task": "Too often", "cron": "* * * * *" })).is_err());
        assert!(schedule.call(&json!({ "task": "Neither" })).is_err());
        assert!(schedule.call(&json!({ "task": "Both", "cron": "0 9 * * *", "at": "2099-01-01 09:00" })).is_err());
        assert!(schedule.call(&json!({ "task": "Once", "at": "2099-01-01 09:00" })).unwrap().contains("number 2"));

        let listed = ListSchedules { database: database.clone(), origin: origin.clone() }.call(&json!({})).unwrap();
        assert!(listed.contains("repeats on \"0 9 * * 1-5\": Check the queue"));
        assert!(listed.contains("runs once: Once"));

        let from_elsewhere = CancelSchedule { database: database.clone(), origin: elsewhere };
        assert!(from_elsewhere.call(&json!({ "number": 1 })).is_err());
        let cancel = CancelSchedule { database: database.clone(), origin: origin.clone() };
        assert_eq!(cancel.call(&json!({ "number": 1 })).unwrap(), "Cancelled number 1.");
        assert!(!ListSchedules { database, origin }.call(&json!({})).unwrap().contains("Check the queue"));

        std::fs::remove_dir_all(directory).unwrap();
    }
}
