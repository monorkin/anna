//! `anna backup` and `anna restore`: everything that makes this Anna this
//! Anna, in one zip.
//!
//! What goes in: the config with her style and CLAUDE.md, her tokens, the
//! database of schedules and the work board, which messages each source has
//! already seen, the log, every thread's session and the Claude transcript
//! behind it — without those a restored thread would have forgotten its
//! conversation — and an export of what she has learned.
//!
//! A backup can be taken while she runs. The database is the only file that
//! could be caught mid-write, and it is snapshotted through SQLite rather
//! than copied. Everything else is small and either appended to or replaced
//! whole.
//!
//! The zip holds tokens in the clear unless told not to, so it is written
//! readable only by its owner. Inside it, transcripts are filed under the
//! thread's name, not under the folder Claude Code derives from an absolute
//! path, so a backup restores under a different home directory.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::clock;
use crate::config;
use crate::control;
use crate::fsutil;
use crate::paths;
use crate::secrets;
use crate::store::Store;

const MANIFEST: &str = "manifest.json";
const SECRETS: &str = "secrets.json";
const MEMORY: &str = "memory.zip";
const CONFIG_FILES: [&str; 3] = ["config.json", "style.md", "CLAUDE.md"];
const SECRET_NAMES: [&str; 1] = [config::JEV_API_KEY];

pub fn backup(to: Option<PathBuf>, without_secrets: bool) -> Result<()> {
    let path = to.unwrap_or_else(|| PathBuf::from(format!("anna-backup-{}.zip", clock::timestamp().replace(':', ""))));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("could not create {}", path.display()))?;

    let mut archive = Archive { zip: ZipWriter::new(file), entries: 0 };
    archive.add_config()?;
    if !without_secrets {
        archive.add_secrets()?;
    }
    archive.add_database()?;
    archive.add_data_files()?;
    archive.add_transcripts()?;
    archive.add_memory()?;
    archive.add(MANIFEST, manifest(without_secrets).to_string().as_bytes())?;
    let entries = archive.entries;
    archive.zip.finish()?;

    println!("Backed up {entries} files to {}.", path.display());
    if !without_secrets {
        println!("It holds Anna's tokens in the clear. Keep it somewhere only you can read.");
    }
    Ok(())
}

fn manifest(without_secrets: bool) -> Value {
    json!({
        "format": 1,
        "anna": env!("CARGO_PKG_VERSION"),
        "created": clock::timestamp(),
        "secrets": !without_secrets,
    })
}

struct Archive {
    zip: ZipWriter<File>,
    entries: usize,
}

impl Archive {
    fn add_config(&mut self) -> Result<()> {
        for name in CONFIG_FILES {
            self.add_file_if_there(&paths::config_dir().join(name), &format!("config/{name}"))?;
        }
        Ok(())
    }

    /// From wherever each one lives — keyring or file — so restoring doesn't
    /// depend on the keyring having come along.
    fn add_secrets(&mut self) -> Result<()> {
        let held: BTreeMap<&str, String> = SECRET_NAMES
            .iter()
            .filter_map(|name| secrets::load(name).map(|value| (*name, value)))
            .collect();
        if !held.is_empty() {
            self.add(SECRETS, serde_json::to_string_pretty(&held)?.as_bytes())?;
        }
        Ok(())
    }

    fn add_database(&mut self) -> Result<()> {
        let database = paths::database();
        if database.exists() {
            let snapshot = std::env::temp_dir().join(format!("anna-snapshot-{}.db", std::process::id()));
            Store::open_at(&database)?.snapshot_to(&snapshot)?;
            let added = self.add_file_if_there(&snapshot, "data/anna.db");
            let _ = fs::remove_file(&snapshot);
            added?;
        }
        Ok(())
    }

    fn add_data_files(&mut self) -> Result<()> {
        self.add_file_if_there(&paths::log_file(), "data/log.jsonl")?;
        self.add_tree(&paths::data_dir().join("threads"), "data/threads")?;
        self.add_tree(&paths::data_dir().join("sources"), "data/sources")
    }

    fn add_transcripts(&mut self) -> Result<()> {
        for thread in names_in(&paths::data_dir().join("threads")) {
            let transcripts = transcripts_of(&thread);
            self.add_tree(&transcripts, &format!("transcripts/{thread}"))?;
        }
        Ok(())
    }

    /// katami writes its own export, which restore hands back to it. A store
    /// with nothing in it has nothing to export, and that is not an error.
    fn add_memory(&mut self) -> Result<()> {
        let exported = std::env::temp_dir().join(format!("anna-memory-{}.zip", std::process::id()));
        let _ = fs::remove_file(&exported);
        if katami::transfer::export("all", Some(exported.clone())).is_ok() {
            self.add_file_if_there(&exported, MEMORY)?;
        }
        let _ = fs::remove_file(&exported);
        Ok(())
    }

    fn add_tree(&mut self, directory: &Path, under: &str) -> Result<()> {
        let Ok(entries) = fs::read_dir(directory) else {
            return Ok(());
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                self.add_tree(&path, &format!("{under}/{name}"))?;
            } else {
                self.add_file_if_there(&path, &format!("{under}/{name}"))?;
            }
        }
        Ok(())
    }

    fn add_file_if_there(&mut self, path: &Path, name: &str) -> Result<()> {
        match fs::read(path) {
            Ok(contents) => self.add(name, &contents),
            Err(_) => Ok(()),
        }
    }

    fn add(&mut self, name: &str, contents: &[u8]) -> Result<()> {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        self.zip.start_file(name, options)?;
        self.zip.write_all(contents)?;
        self.entries += 1;
        Ok(())
    }
}

pub fn restore(path: &Path, force: bool) -> Result<()> {
    if control::is_running() {
        bail!("Anna is running; `anna stop` first, then restore");
    }
    let already_here = paths::config_dir().join("config.json").exists() || paths::database().exists();
    if already_here && !force {
        bail!("there is already an Anna set up here; pass --force to replace her with the backup");
    }

    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut zip = ZipArchive::new(file).context("that is not a zip file")?;
    let manifest: Value = serde_json::from_slice(&contents_of(&mut zip, MANIFEST).context("that zip is not an Anna backup")?)?;
    if manifest["format"] != json!(1) {
        bail!("that backup is in a format this Anna doesn't know");
    }

    let mut restored = 0;
    for index in 0..zip.len() {
        let name = zip.by_index(index)?.name().to_string();
        let contents = contents_of(&mut zip, &name)?;

        if name == SECRETS {
            for (secret, value) in serde_json::from_slice::<BTreeMap<String, String>>(&contents)? {
                secrets::store(&secret, &value)?;
            }
            restored += 1;
        } else if name == MEMORY {
            restore_memory(&contents)?;
            restored += 1;
        } else if let Some(destination) = destination_of(&name)? {
            put(&destination, &contents, name.starts_with("config/"))?;
            restored += 1;
        }
    }

    println!("Restored {restored} files from the backup of {}.", manifest["created"].as_str().unwrap_or("an unknown time"));
    println!("`anna start` brings her back up.");
    Ok(())
}

fn contents_of(zip: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut contents = Vec::new();
    zip.by_name(name)?.read_to_end(&mut contents)?;
    Ok(contents)
}

/// Where an entry goes, or nowhere for the manifest and anything this Anna
/// doesn't recognize. An entry that tries to climb out of its folder is an
/// error: a backup is a file someone can hand you.
fn destination_of(name: &str) -> Result<Option<PathBuf>> {
    let path = Path::new(name);
    if !path.components().all(|it| matches!(it, Component::Normal(_))) {
        bail!("the backup holds a file with an unsafe name: {name}");
    }

    let mut parts = path.components().map(|it| it.as_os_str().to_string_lossy().into_owned());
    let destination = match parts.next().as_deref() {
        Some("config") => Some(paths::config_dir().join(parts.collect::<PathBuf>())),
        Some("data") => Some(paths::data_dir().join(parts.collect::<PathBuf>())),
        Some("transcripts") => parts
            .next()
            .map(|thread| transcripts_of(&thread).join(parts.collect::<PathBuf>())),
        _ => None,
    };
    Ok(destination)
}

fn put(destination: &Path, contents: &[u8], steers_anna: bool) -> Result<()> {
    if steers_anna {
        fsutil::write_private(destination, &String::from_utf8_lossy(contents))
    } else {
        if let Some(directory) = destination.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(destination, contents)?;
        Ok(())
    }
}

/// Memories that are already here win over the backup's: they are the newer
/// ones, and katami's store is shared with every other session on the
/// machine.
fn restore_memory(exported: &[u8]) -> Result<()> {
    let path = std::env::temp_dir().join(format!("anna-memory-{}.zip", std::process::id()));
    fs::write(&path, exported)?;
    let imported = katami::transfer::import(&path, katami::transfer::OnCollision::Skip, &paths::claude_config_home());
    let _ = fs::remove_file(&path);
    imported
}

/// Claude Code keeps a folder's transcripts under the folder's absolute path
/// with everything but letters and digits turned into dashes.
fn transcripts_of(thread: &str) -> PathBuf {
    let folder: String = paths::thread_dir(thread)
        .to_string_lossy()
        .chars()
        .map(|it| if it.is_ascii_alphanumeric() { it } else { '-' })
        .collect();
    paths::claude_config_home().join("projects").join(folder)
}

fn names_in(directory: &Path) -> Vec<String> {
    match fs::read_dir(directory) {
        Ok(entries) => entries.flatten().map(|it| it.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_land_where_they_belong_and_nowhere_else() {
        assert_eq!(destination_of("config/style.md").unwrap(), Some(paths::config_dir().join("style.md")));
        assert_eq!(
            destination_of("data/threads/basecamp-card-1/session").unwrap(),
            Some(paths::data_dir().join("threads/basecamp-card-1/session"))
        );
        assert_eq!(
            destination_of("transcripts/terminal-main/abc.jsonl").unwrap(),
            Some(transcripts_of("terminal-main").join("abc.jsonl"))
        );
        assert_eq!(destination_of("manifest.json").unwrap(), None);
        assert_eq!(destination_of("something/else").unwrap(), None);

        assert!(destination_of("config/../../.ssh/authorized_keys").is_err());
        assert!(destination_of("/etc/passwd").is_err());
    }

    #[test]
    fn transcripts_are_found_the_way_claude_code_names_them() {
        let folder = transcripts_of("terminal-main");
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();

        assert!(name.starts_with('-'));
        assert!(name.ends_with("-threads-terminal-main"));
        assert!(name.chars().all(|it| it.is_ascii_alphanumeric() || it == '-'));
    }
}
