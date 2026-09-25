//! What Anna has to remember to run, as opposed to what she has learned:
//! the schedules her threads set up, what each thread is working on, and the
//! messages they send each other. One SQLite file, so a restart loses
//! nothing and the next thing she needs to keep is a table away.
//!
//! This is not katami's memory. That holds durable facts and is shared with
//! every other session on the machine; this holds Anna's own moving parts.
//!
//! The schema moves forward through numbered migrations keyed on
//! `user_version`. Every thread and the scheduler open their own connection;
//! WAL and a busy timeout let them write side by side.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::conversation::Origin;

const MIGRATIONS: [&str; 6] = ["
    CREATE TABLE schedules (
        id INTEGER PRIMARY KEY,
        source TEXT NOT NULL,
        conversation TEXT NOT NULL,
        cron TEXT,
        task TEXT NOT NULL,
        next_run INTEGER NOT NULL,
        created TEXT NOT NULL
    );
    CREATE INDEX schedules_by_next_run ON schedules (next_run);

    CREATE TABLE work (
        source TEXT NOT NULL,
        conversation TEXT NOT NULL,
        thread TEXT NOT NULL,
        title TEXT NOT NULL,
        project TEXT,
        status TEXT NOT NULL DEFAULT 'open',
        updated TEXT NOT NULL,
        PRIMARY KEY (source, conversation)
    );

    CREATE TABLE mail (
        id INTEGER PRIMARY KEY,
        from_thread TEXT NOT NULL,
        to_source TEXT NOT NULL,
        to_conversation TEXT NOT NULL,
        body TEXT NOT NULL,
        sent INTEGER NOT NULL,
        delivered INTEGER
    );
", "
    ALTER TABLE schedules ADD COLUMN trusted INTEGER NOT NULL DEFAULT 0;
", "
    CREATE TABLE turns (
        id INTEGER PRIMARY KEY,
        source TEXT NOT NULL,
        conversation TEXT NOT NULL,
        trusted INTEGER NOT NULL,
        said TEXT NOT NULL,
        queued TEXT NOT NULL
    );
", "
    CREATE TABLE opened (
        source TEXT NOT NULL,
        conversation TEXT NOT NULL,
        opened TEXT NOT NULL,
        PRIMARY KEY (source, conversation)
    );
", "
    ALTER TABLE mail ADD COLUMN trusted INTEGER NOT NULL DEFAULT 0;
", "
    ALTER TABLE work ADD COLUMN about TEXT;
"];

/// A turn that was asked for and not yet finished: what a stopped Anna
/// picks up again when she starts.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTurn {
    pub id: i64,
    pub origin: Origin,
    pub trusted: bool,
    pub said: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: i64,
    pub origin: Origin,
    /// A cron expression, or none for something that runs once.
    pub cron: Option<String>,
    pub task: String,
    pub next_run: i64,
    /// Whether the turn that set it up ran on a trusted person's word. It
    /// runs with that standing and no more, so nobody schedules their way to
    /// something they couldn't ask for directly.
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub origin: Origin,
    pub thread: String,
    pub title: String,
    pub project: Option<String>,
    /// The id of the to-do, card or message the work is on, when that isn't
    /// the conversation the thread lives in: what is said there is routed to
    /// this thread rather than to one that would only pass it on.
    pub about: Option<String>,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    pub id: i64,
    pub from_thread: String,
    pub to: Origin,
    pub body: String,
    /// Whether it was sent from a turn a trusted person started. That turn
    /// could have done the work itself, so its word carries to the thread
    /// that owns the work — the same Anna, reading with the same care.
    pub trusted: bool,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open_at(path: &Path) -> Result<Store> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        let connection = Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;

        let store = Store { connection };
        store.migrate()?;
        Ok(store)
    }

    /// A consistent copy of the whole database, safe to take while threads
    /// and the scheduler are writing: SQLite builds it from one read
    /// transaction. Copying the file instead could catch it mid-write, and
    /// would miss whatever is still in the write-ahead log.
    pub fn snapshot_to(&self, path: &Path) -> Result<()> {
        let _ = std::fs::remove_file(path);
        self.connection
            .execute("VACUUM INTO ?1", params![path.to_string_lossy()])
            .with_context(|| format!("could not snapshot the database to {}", path.display()))?;
        Ok(())
    }

    /// The version is read inside the transaction that acts on it, and the
    /// transaction takes the write lock up front: two processes opening a
    /// new database at once would otherwise both see version 0, and the
    /// second would fail on tables the first had just made. A database from
    /// a newer Anna is refused rather than written to with an old idea of
    /// what is in it.
    fn migrate(&self) -> Result<()> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let migrated = self.migrate_from_where_it_is();
        if migrated.is_ok() {
            self.connection.execute_batch("COMMIT")?;
        } else {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
        migrated
    }

    fn migrate_from_where_it_is(&self) -> Result<()> {
        let version: usize = self.connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > MIGRATIONS.len() {
            bail!("the database is from a newer Anna (version {version}, and this one knows {}); update her before starting", MIGRATIONS.len());
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            self.connection.execute_batch(&format!("{migration} PRAGMA user_version = {};", index + 1))?;
        }
        Ok(())
    }

    pub fn add_schedule(&self, origin: &Origin, cron: Option<&str>, task: &str, next_run: i64, trusted: bool, created: &str) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO schedules (source, conversation, cron, task, next_run, trusted, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![origin.source, origin.conversation, cron, task, next_run, trusted, created],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn schedules_of(&self, origin: &Origin) -> Result<Vec<Schedule>> {
        self.schedules_where("source = ?1 AND conversation = ?2", params![origin.source, origin.conversation])
    }

    pub fn schedules_due(&self, now: i64) -> Result<Vec<Schedule>> {
        self.schedules_where("next_run <= ?1", params![now])
    }

    pub fn run_again_at(&self, id: i64, next_run: i64) -> Result<()> {
        self.connection
            .execute("UPDATE schedules SET next_run = ?2 WHERE id = ?1", params![id, next_run])?;
        Ok(())
    }

    pub fn remove_schedule(&self, id: i64) -> Result<()> {
        self.connection.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// A thread can only cancel what it set up itself.
    pub fn cancel_schedule(&self, id: i64, origin: &Origin) -> Result<bool> {
        let removed = self.connection.execute(
            "DELETE FROM schedules WHERE id = ?1 AND source = ?2 AND conversation = ?3",
            params![id, origin.source, origin.conversation],
        )?;
        Ok(removed > 0)
    }

    fn schedules_where(&self, condition: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Vec<Schedule>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT id, source, conversation, cron, task, next_run, trusted FROM schedules WHERE {condition} ORDER BY next_run, id"
        ))?;
        let schedules = statement
            .query_map(values, |row| {
                Ok(Schedule {
                    id: row.get(0)?,
                    origin: Origin { source: row.get(1)?, conversation: row.get(2)? },
                    cron: row.get(3)?,
                    task: row.get(4)?,
                    next_run: row.get(5)?,
                    trusted: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(schedules)
    }

    /// Written when a turn is queued and taken out when it is over, so what
    /// was still to do survives a stop, a crash, or a limit that was hit.
    pub fn keep_turn(&self, origin: &Origin, trusted: bool, said: &str, now: &str) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO turns (source, conversation, trusted, said, queued) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![origin.source, origin.conversation, trusted, said, now],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn forget_turn(&self, id: i64) -> Result<()> {
        self.connection.execute("DELETE FROM turns WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// In the order they were asked for, which is the order they run in.
    pub fn turns_left(&self) -> Result<Vec<PendingTurn>> {
        let mut statement = self.connection.prepare("SELECT id, source, conversation, trusted, said FROM turns ORDER BY id")?;
        let turns = statement
            .query_map([], |row| {
                Ok(PendingTurn {
                    id: row.get(0)?,
                    origin: Origin { source: row.get(1)?, conversation: row.get(2)? },
                    trusted: row.get(3)?,
                    said: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(turns)
    }

    /// A conversation is opened the first time a trusted person speaks in
    /// it, and stays opened. Anyone else is heard there and nowhere else, so
    /// a reply on work she was given reaches her without the whole account
    /// being able to hand her work.
    pub fn open_conversation(&self, origin: &Origin, now: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO opened (source, conversation, opened) VALUES (?1, ?2, ?3)",
            params![origin.source, origin.conversation, now],
        )?;
        Ok(())
    }

    pub fn opened(&self, origin: &Origin) -> Result<bool> {
        let opened = self
            .connection
            .query_row(
                "SELECT 1 FROM opened WHERE source = ?1 AND conversation = ?2",
                params![origin.source, origin.conversation],
                |_| Ok(()),
            )
            .optional()?;
        Ok(opened.is_some())
    }

    /// One line per conversation: claiming again replaces what was claimed.
    pub fn claim_work(&self, origin: &Origin, thread: &str, title: &str, project: Option<&str>, about: Option<&str>, now: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO work (source, conversation, thread, title, project, about, status, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7)
             ON CONFLICT (source, conversation) DO UPDATE SET
                 thread = excluded.thread, title = excluded.title, project = excluded.project,
                 about = coalesce(excluded.about, work.about), status = 'open', updated = excluded.updated",
            params![origin.source, origin.conversation, thread, title, project, about, now],
        )?;
        Ok(())
    }

    /// The thread that has claimed work on a recording, by the id it named.
    pub fn thread_working_on(&self, about: &str) -> Result<Option<(String, Origin)>> {
        Ok(self
            .connection
            .query_row(
                "SELECT thread, source, conversation FROM work WHERE status = 'open' AND about = ?1 ORDER BY updated DESC LIMIT 1",
                params![about],
                |row| Ok((row.get(0)?, Origin { source: row.get(1)?, conversation: row.get(2)? })),
            )
            .optional()?)
    }

    pub fn finish_work(&self, origin: &Origin, now: &str) -> Result<bool> {
        let finished = self.connection.execute(
            "UPDATE work SET status = 'done', updated = ?3 WHERE source = ?1 AND conversation = ?2 AND status = 'open'",
            params![origin.source, origin.conversation, now],
        )?;
        Ok(finished > 0)
    }

    /// Everything still claimed, newest first — the whole board, for anyone
    /// asking from outside a thread.
    pub fn open_work(&self) -> Result<Vec<Work>> {
        let mut statement = self.connection.prepare(
            "SELECT source, conversation, thread, title, project, about, updated FROM work
             WHERE status = 'open' ORDER BY updated DESC",
        )?;
        let work = statement
            .query_map([], |row| {
                Ok(Work {
                    origin: Origin { source: row.get(0)?, conversation: row.get(1)? },
                    thread: row.get(2)?,
                    title: row.get(3)?,
                    project: row.get(4)?,
                    about: row.get(5)?,
                    updated: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(work)
    }

    pub fn open_work_of_others(&self, origin: &Origin) -> Result<Vec<Work>> {
        let mut statement = self.connection.prepare(
            "SELECT source, conversation, thread, title, project, about, updated FROM work
             WHERE status = 'open' AND NOT (source = ?1 AND conversation = ?2) ORDER BY updated DESC",
        )?;
        let work = statement
            .query_map(params![origin.source, origin.conversation], |row| {
                Ok(Work {
                    origin: Origin { source: row.get(0)?, conversation: row.get(1)? },
                    thread: row.get(2)?,
                    title: row.get(3)?,
                    project: row.get(4)?,
                    about: row.get(5)?,
                    updated: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(work)
    }

    /// Where a thread lives, for anyone who only knows its name.
    pub fn origin_of_thread(&self, thread: &str) -> Result<Option<Origin>> {
        let origin = self
            .connection
            .query_row("SELECT source, conversation FROM work WHERE thread = ?1", params![thread], |row| {
                Ok(Origin { source: row.get(0)?, conversation: row.get(1)? })
            })
            .optional()?;
        Ok(origin)
    }

    /// Every thread that has ever been on the board, done or not, whose name
    /// has `part` in it.
    pub fn threads_named(&self, part: &str) -> Result<Vec<(String, Origin)>> {
        let mut statement = self
            .connection
            .prepare("SELECT thread, source, conversation FROM work WHERE instr(thread, ?1) > 0 ORDER BY thread")?;
        let threads = statement
            .query_map(params![part], |row| Ok((row.get(0)?, Origin { source: row.get(1)?, conversation: row.get(2)? })))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(threads)
    }

    pub fn send_mail(&self, from_thread: &str, to: &Origin, body: &str, trusted: bool, now: i64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO mail (from_thread, to_source, to_conversation, body, trusted, sent) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![from_thread, to.source, to.conversation, body, trusted, now],
        )?;
        Ok(())
    }

    pub fn mail_sent_since(&self, from_thread: &str, since: i64) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM mail WHERE from_thread = ?1 AND sent >= ?2",
            params![from_thread, since],
            |row| row.get(0),
        )?)
    }

    pub fn undelivered_mail(&self) -> Result<Vec<Mail>> {
        let mut statement = self.connection.prepare(
            "SELECT id, from_thread, to_source, to_conversation, body, trusted FROM mail WHERE delivered IS NULL ORDER BY id",
        )?;
        let mail = statement
            .query_map([], |row| {
                Ok(Mail {
                    id: row.get(0)?,
                    from_thread: row.get(1)?,
                    to: Origin { source: row.get(2)?, conversation: row.get(3)? },
                    body: row.get(4)?,
                    trusted: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(mail)
    }

    pub fn mark_delivered(&self, id: i64, now: i64) -> Result<()> {
        self.connection
            .execute("UPDATE mail SET delivered = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!("anna-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        (Store::open_at(&directory.join("anna.db")).unwrap(), directory)
    }

    fn origin(conversation: &str) -> Origin {
        Origin { source: "basecamp".to_string(), conversation: conversation.to_string() }
    }

    #[test]
    fn turns_are_kept_until_they_are_over_in_the_order_they_came() {
        let (store, directory) = store("turns");

        let first = store.keep_turn(&origin("card-1"), true, "fix the login", "2026-09-22T09:00:00Z").unwrap();
        let second = store.keep_turn(&origin("card-2"), false, "look at the cookie", "2026-09-22T09:00:01Z").unwrap();
        let left = store.turns_left().unwrap();
        assert_eq!(left.iter().map(|it| it.said.as_str()).collect::<Vec<_>>(), ["fix the login", "look at the cookie"]);
        assert!(left[0].trusted && !left[1].trusted);
        assert_eq!(left[1].origin, origin("card-2"));

        store.forget_turn(first).unwrap();
        assert_eq!(store.turns_left().unwrap().iter().map(|it| it.id).collect::<Vec<_>>(), [second]);
        store.forget_turn(second).unwrap();
        assert!(store.turns_left().unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schedules_come_due_move_on_and_belong_to_whoever_made_them() {
        let (store, directory) = store("schedules");
        let daily = store.add_schedule(&origin("card-1"), Some("0 9 * * *"), "Check the deploy", 1_000, true, "now").unwrap();
        let once = store.add_schedule(&origin("card-2"), None, "Remind Marta", 2_000, false, "now").unwrap();
        assert!(store.schedules_of(&origin("card-1")).unwrap()[0].trusted);
        assert!(!store.schedules_of(&origin("card-2")).unwrap()[0].trusted);

        assert_eq!(store.schedules_due(999).unwrap(), []);
        assert_eq!(store.schedules_due(1_500).unwrap().iter().map(|it| it.id).collect::<Vec<_>>(), [daily]);

        store.run_again_at(daily, 90_000).unwrap();
        assert_eq!(store.schedules_due(2_500).unwrap().iter().map(|it| it.id).collect::<Vec<_>>(), [once]);

        assert!(!store.cancel_schedule(daily, &origin("card-2")).unwrap());
        assert!(store.cancel_schedule(daily, &origin("card-1")).unwrap());
        assert_eq!(store.schedules_of(&origin("card-1")).unwrap(), []);
        assert_eq!(store.schedules_of(&origin("card-2")).unwrap()[0].task, "Remind Marta");

        drop(store);
        let reopened = Store::open_at(&directory.join("anna.db")).unwrap();
        assert_eq!(reopened.schedules_of(&origin("card-2")).unwrap().len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_conversation_stays_opened_once_a_trusted_person_spoke_in_it() {
        let (store, directory) = store("opened");
        assert!(!store.opened(&origin("card-1")).unwrap());

        store.open_conversation(&origin("card-1"), "t1").unwrap();
        store.open_conversation(&origin("card-1"), "t2").unwrap();
        assert!(store.opened(&origin("card-1")).unwrap());
        assert!(!store.opened(&origin("card-2")).unwrap());
        assert!(!store.opened(&Origin { source: "terminal".to_string(), conversation: "card-1".to_string() }).unwrap(), "opened on one source, not every source");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_thread_sees_what_the_others_have_open_but_not_its_own() {
        let (store, directory) = store("work");
        store.claim_work(&origin("card-1"), "basecamp-card-1", "Login 500s on Safari", Some("frontdesk"), None, "t1").unwrap();
        store.claim_work(&origin("card-2"), "basecamp-card-2", "Session cookie dropped", Some("frontdesk"), Some("10337671931"), "t2").unwrap();
        store.claim_work(&origin("card-2"), "basecamp-card-2", "Session cookie dropped on Safari", None, None, "t3").unwrap();

        let seen_by_first = store.open_work_of_others(&origin("card-1")).unwrap();
        assert_eq!(seen_by_first.len(), 1);
        assert_eq!(seen_by_first[0].title, "Session cookie dropped on Safari");
        assert_eq!(seen_by_first[0].project, None);
        assert_eq!(seen_by_first[0].about.as_deref(), Some("10337671931"), "claiming again without saying what it is about keeps what it was about");
        assert_eq!(store.thread_working_on("10337671931").unwrap(), Some(("basecamp-card-2".to_string(), origin("card-2"))), "what is said on that to-do is card-2's to hear");
        assert_eq!(store.thread_working_on("1").unwrap(), None);
        assert_eq!(store.origin_of_thread("basecamp-card-2").unwrap(), Some(origin("card-2")));
        assert_eq!(store.origin_of_thread("nobody").unwrap(), None);

        assert!(store.finish_work(&origin("card-2"), "t4").unwrap());
        assert!(!store.finish_work(&origin("card-2"), "t5").unwrap());
        assert_eq!(store.open_work_of_others(&origin("card-1")).unwrap(), []);

        assert_eq!(store.threads_named("card-2").unwrap(), [("basecamp-card-2".to_string(), origin("card-2"))], "done work can still be found");
        assert_eq!(store.threads_named("card").unwrap().len(), 2);
        assert_eq!(store.threads_named("nobody").unwrap(), []);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mail_waits_until_it_is_delivered() {
        let (store, directory) = store("mail");
        store.send_mail("basecamp-card-1", &origin("card-2"), "Same bug as mine, I'll take both.", false, 100).unwrap();
        store.send_mail("basecamp-card-1", &origin("card-2"), "Fixed in 4f2a.", true, 200).unwrap();

        assert_eq!(store.mail_sent_since("basecamp-card-1", 150).unwrap(), 1);
        let waiting = store.undelivered_mail().unwrap();
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].to, origin("card-2"));
        assert!(!waiting[0].trusted && waiting[1].trusted, "each carries the standing it was sent on");

        store.mark_delivered(waiting[0].id, 300).unwrap();
        assert_eq!(store.undelivered_mail().unwrap()[0].body, "Fixed in 4f2a.");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
