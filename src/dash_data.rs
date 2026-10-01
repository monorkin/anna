//! What `anna dash` shows, gathered in one place: her Claude accounts and
//! Jev's bill, what is going on now, and the end of her log. Gathering asks
//! the running Anna, ax and the log; turning their answers into rows touches
//! none of them, and that is the part the tests cover.

use ax::usage::Report;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

use crate::config;
use crate::control;
use crate::judge::JEV_DOLLARS_PER_REQUEST_ESTIMATE;
use crate::lifecycle::text;
use crate::logs::{self, short_id};
use crate::paths;
use crate::secrets;

const LOG_LINES: usize = 500;

/// Everything on the dashboard at one moment.
pub struct Snapshot {
    pub accounts: Vec<Account>,
    /// Why there are no accounts to show, when ax couldn't say.
    pub accounts_unknown: Option<String>,
    /// Jev's requests and what they cost, as rows, when it is set up.
    pub jev: Option<Vec<[String; 2]>>,
    /// The newest lines of the log, as `anna log` colours them.
    pub log: Vec<String>,
    /// Or why it can't be had: she isn't running, or didn't answer.
    pub now: Result<Now, String>,
}

/// One Claude account and how full its limits are.
#[derive(Debug, PartialEq)]
pub struct Account {
    /// Its alias, or its email up to the @.
    pub name: String,
    pub in_use: bool,
    /// Session, week and the model-scoped one, always three and in that
    /// order; a limit ax has nothing on is None.
    pub limits: [Option<Limit>; 3],
}

#[derive(Debug, PartialEq)]
pub struct Limit {
    pub name: String,
    pub percent: f64,
    /// How long until it resets, or empty when that isn't known.
    pub resets_in: String,
}

/// What `anna status` knows, as rows.
#[derive(Default, Debug, PartialEq)]
pub struct Now {
    /// Thread, running for, messages waiting, title, hand, the hand's
    /// worktree and how long it has run: one row per hand, the thread's
    /// columns only on its first.
    pub going_on: Vec<[String; 7]>,
    /// Thread and what it's doing.
    pub claimed: Vec<[String; 2]>,
    /// Every thread that can be told something, the busy ones first.
    pub threads: Vec<String>,
}

/// Asked once: the key lives in the keyring, which isn't something to knock
/// on every few seconds.
pub fn jev_is_set_up() -> bool {
    secrets::load(config::JEV_API_KEY).is_some()
}

pub fn gather(with_jev: bool) -> Snapshot {
    let (accounts, accounts_unknown) = match ax::usage::of_every_account() {
        Ok(reports) => (accounts_of(&reports, ax::clock::now_seconds()), None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };

    Snapshot {
        accounts,
        accounts_unknown,
        jev: with_jev.then(|| jev_rows(&JevRequests::in_log(&fs::read_to_string(paths::log_file()).unwrap_or_default(), &a_day_ago()))),
        log: logs::newest_styled(LOG_LINES),
        now: control::ask(json!({ "command": "status" })).map(|it| now_of(&it["status"])).map_err(|error| format!("{error}.")),
    }
}

fn a_day_ago() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(24)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// How many requests went to Jev, from the log.
#[derive(Default, Debug, PartialEq)]
struct JevRequests {
    since: u64,
    ever: u64,
}

impl JevRequests {
    /// From `since` on, and over the whole log. Lines that aren't JSON are
    /// skipped.
    fn in_log(log: &str, since: &str) -> JevRequests {
        let mut requests = JevRequests::default();
        let asked = log.lines().filter(|it| it.contains("judge.jev_asked")).filter_map(|it| serde_json::from_str::<Value>(it).ok());
        for entry in asked.filter(|it| it["event"] == "judge.jev_asked") {
            requests.ever += 1;
            if entry["at"].as_str().unwrap_or_default() >= since {
                requests.since += 1;
            }
        }
        requests
    }
}

/// Each account's limits, in the order and under the names ax gives them.
fn accounts_of(reports: &[Report], now: i64) -> Vec<Account> {
    reports
        .iter()
        .map(|report| {
            let mut limits = [None, None, None];
            if let Some(reading) = &report.reading {
                for (slot, (name, window)) in limits.iter_mut().zip(ax::usage::limits_of(&reading.usage)) {
                    *slot = window.map(|window| Limit {
                        name: name.to_string(),
                        percent: window.utilization,
                        resets_in: window
                            .resets_at
                            .as_deref()
                            .and_then(ax::clock::epoch_seconds_of)
                            .map(|at| ax::clock::span(at - now))
                            .unwrap_or_default(),
                    });
                }
            }
            Account { name: name_of(&report.account), in_use: report.active, limits }
        })
        .collect()
}

fn name_of(account: &ax::store::Account) -> String {
    match &account.alias {
        Some(alias) => alias.clone(),
        None => account.email.split('@').next().unwrap_or_default().to_string(),
    }
}

fn jev_rows(requests: &JevRequests) -> Vec<[String; 2]> {
    vec![
        ["Requests, last day".to_string(), requests.since.to_string()],
        ["Requests, whole log".to_string(), requests.ever.to_string()],
        ["Cost, last day (est.)".to_string(), dollars(requests.since)],
        ["Cost, whole log (est.)".to_string(), dollars(requests.ever)],
    ]
}

fn dollars(requests: u64) -> String {
    format!("${:.2}", requests as f64 * JEV_DOLLARS_PER_REQUEST_ESTIMATE)
}

/// The `status` answer of a running Anna, as rows.
fn now_of(status: &Value) -> Now {
    let mut now = Now::default();
    for turn in listed(&status["going_on"]) {
        let thread = text(&turn["conversation"]).to_string();
        let turn_columns = [short_id(&thread), text(&turn["for"]).to_string(), waiting(&turn["waiting"]), text(&turn["doing"]).to_string()];
        let hands = listed(&turn["hands"]);
        if hands.is_empty() {
            let [thread, running, waiting, title] = turn_columns.clone();
            now.going_on.push([thread, running, waiting, title, String::new(), String::new(), String::new()]);
        }
        for (index, hand) in hands.iter().enumerate() {
            let [thread, running, waiting, title] = if index == 0 { turn_columns.clone() } else { Default::default() };
            let worktree = Path::new(text(&hand["project"])).file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
            now.going_on.push([thread, running, waiting, title, text(&hand["hand"]).to_string(), worktree, text(&hand["for"]).to_string()]);
        }
        now.threads.push(thread);
    }
    for work in listed(&status["board"]) {
        let thread = text(&work["conversation"]).to_string();
        now.claimed.push([short_id(&thread), text(&work["doing"]).to_string()]);
        now.threads.push(thread);
    }
    now
}

fn listed(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn waiting(value: &Value) -> String {
    match value.as_u64() {
        Some(0) | None => String::new(),
        Some(count) => count.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jev_requests_are_counted_from_a_moment_on_and_over_the_whole_log() {
        let log = [
            r#"{"at":"2026-09-26T09:00:00Z","event":"judge.jev_asked","details":{"bytes":10}}"#,
            r#"{"at":"2026-09-27T09:00:00Z","event":"judge.jev_asked","details":{"bytes":10}}"#,
            r#"{"at":"2026-09-27T09:00:01Z","event":"thread.woken","details":{"said":"judge.jev_asked"}}"#,
            "judge.jev_asked, not json",
        ]
        .join("\n");

        let requests = JevRequests::in_log(&log, "2026-09-27T00:00:00Z");
        assert_eq!(requests, JevRequests { since: 1, ever: 2 });
        let rows = jev_rows(&requests);
        assert_eq!(rows[0], ["Requests, last day".to_string(), "1".to_string()]);
        assert_eq!(rows[1][1], "2");
        assert_eq!(rows[3][1], dollars(2));
        assert_eq!(dollars(2500), format!("${:.2}", 2500.0 * JEV_DOLLARS_PER_REQUEST_ESTIMATE));
    }

    #[test]
    fn accounts_show_each_limit_and_which_one_is_in_use() {
        let account = |email: &str| serde_json::from_value(json!({ "number": 1, "email": email, "added": "2026-09-01" })).unwrap();
        let usage = serde_json::from_value(json!({
            "five_hour": { "utilization": 40.0, "resets_at": "2026-09-27T12:00:00Z" },
            "seven_day": { "utilization": 10.0, "resets_at": null },
        }))
        .unwrap();
        let mut aliased: ax::store::Account = account("work@example.com");
        aliased.alias = Some("Work".to_string());
        let reports = [
            Report { account: account("marta@example.com"), active: true, reading: Some(ax::usage::Reading { usage, taken_at: 1_000 }), failed: None },
            Report { account: aliased, active: false, reading: None, failed: Some("asked too often".to_string()) },
        ];
        let now = ax::clock::epoch_seconds_of("2026-09-27T10:20:00Z").unwrap();

        let accounts = accounts_of(&reports, now);
        assert_eq!(
            accounts[0],
            Account {
                name: "marta".to_string(),
                in_use: true,
                limits: [
                    Some(Limit { name: "session".to_string(), percent: 40.0, resets_in: "1h 40m".to_string() }),
                    Some(Limit { name: "week".to_string(), percent: 10.0, resets_in: String::new() }),
                    None,
                ],
            }
        );
        assert_eq!(accounts[1], Account { name: "Work".to_string(), in_use: false, limits: [None, None, None] });
    }

    #[test]
    fn status_becomes_rows_with_hands_under_their_thread() {
        let status = json!({
            "going_on": [{
                "conversation": "basecamp-card-1",
                "for": "14m",
                "waiting": 3,
                "doing": "Fixing the login",
                "hands": [
                    { "hand": "h18d9", "project": "/srv/worktrees/login-fix", "for": "13m" },
                    { "hand": "h18da", "project": "/srv/worktrees/login-docs", "for": "2m" },
                ],
            }, {
                "conversation": "basecamp-card-3",
                "for": "1m",
                "waiting": 0,
                "doing": null,
                "hands": [],
            }],
            "board": [{ "conversation": "basecamp-card-2", "doing": "Waiting on review" }],
        });

        let row = |cells: [&str; 7]| cells.map(String::from);
        assert_eq!(
            now_of(&status),
            Now {
                going_on: vec![
                    row(["basecamp-card-1", "14m", "3", "Fixing the login", "h18d9", "login-fix", "13m"]),
                    row(["", "", "", "", "h18da", "login-docs", "2m"]),
                    row(["basecamp-card-3", "1m", "", "-", "", "", ""]),
                ],
                claimed: vec![["basecamp-card-2".to_string(), "Waiting on review".to_string()]],
                threads: vec!["basecamp-card-1".to_string(), "basecamp-card-3".to_string(), "basecamp-card-2".to_string()],
            }
        );
        assert_eq!(now_of(&json!({})), Now::default());
    }
}
