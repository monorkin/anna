//! The languages and tools a hand can run.
//!
//! A sandbox sees /usr and nothing from the home directory, which is where
//! mise keeps Ruby, Node, Go and the rest — so without help a hand can edit a
//! project but not run its tests. mise's installs folder is bound read-only
//! and its bin folders go on the hand's PATH.
//!
//! The list comes from running mise in the home directory, never in the
//! project: a hand can write the project's mise.toml, and mise reading that
//! on the host would be the hand running code outside its sandbox. Only
//! folders under the installs folder are used; mise also lists places like
//! cargo's bin folder, whose parent holds registry credentials.
//!
//! Rust doesn't live under mise: rustup keeps its toolchains in a folder of
//! its own, and cargo its downloaded crates in another, next to the
//! person's registry token. The toolchain goes in read-only, the crates go
//! in with a writable layer on top — cargo unpacks into its cache — and
//! never the token. A hand has no network, so it builds offline from what
//! the brain fetched before starting it.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::deadline;
use crate::logs;
use crate::paths;

const SECONDS_FOR_MISE: u64 = 15;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Toolchains {
    pub installs: Option<PathBuf>,
    pub bins: Vec<PathBuf>,
    pub rust: Option<Rust>,
}

/// One rustup toolchain, and cargo's caches.
#[derive(Debug, Clone, PartialEq)]
pub struct Rust {
    pub toolchain: PathBuf,
    /// cargo's `registry` and `git` folders, when they exist.
    pub caches: Vec<PathBuf>,
}

impl Toolchains {
    pub fn discover() -> Toolchains {
        let Some(home) = dirs::home_dir() else {
            return Toolchains::default();
        };
        let mut toolchains = match paths::program("mise") {
            Some(mise) => Toolchains::from_mise(&mise, &home),
            None => Toolchains::default(),
        };
        toolchains.rust = Rust::discover(&home);
        toolchains
    }

    fn from_mise(mise: &Path, home: &Path) -> Toolchains {
        // Hands without mise's tools still work; an Anna that never comes up
        // because mise is busy doesn't
        let mut bin_paths = Command::new(mise);
        bin_paths.arg("bin-paths").current_dir(home);
        match deadline::output_within(&mut bin_paths, Duration::from_secs(SECONDS_FOR_MISE)) {
            Some(output) => Toolchains::listed(&String::from_utf8_lossy(&output.stdout)),
            None => {
                logs::event("toolchains.not_found", json!({ "reason": "mise did not answer in time" }));
                Toolchains::default()
            }
        }
    }

    /// The PATH inside the sandbox: the tools first, then the system's.
    pub fn path(&self) -> String {
        self.bins
            .iter()
            .map(|it| it.to_string_lossy().into_owned())
            .chain(self.rust.iter().map(|it| it.toolchain.join("bin").to_string_lossy().into_owned()))
            .chain(["/usr/bin".to_string()])
            .collect::<Vec<_>>()
            .join(":")
    }

    /// mise keeps its installs wherever its data directory is, so the folder
    /// is read off the first path that names it instead of being assumed.
    fn listed(bin_paths: &str) -> Toolchains {
        match bin_paths.lines().find_map(installs_folder_of) {
            Some(installs) => Toolchains {
                bins: bin_paths
                    .lines()
                    .map(PathBuf::from)
                    .filter(|it| it.starts_with(&installs))
                    .collect(),
                installs: Some(installs),
                rust: None,
            },
            None => Toolchains::default(),
        }
    }
}

impl Rust {
    fn discover(home: &Path) -> Option<Rust> {
        let rustup = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".rustup"));
        let cargo = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
        Rust::in_folders(&rustup, &cargo)
    }

    /// rustup's default toolchain when it is installed, otherwise stable: a
    /// settings file can name a version that was never downloaded.
    fn in_folders(rustup: &Path, cargo: &Path) -> Option<Rust> {
        let toolchains = rustup.join("toolchains");
        let named = std::fs::read_to_string(rustup.join("settings.toml"))
            .ok()
            .and_then(|settings| settings.lines().find_map(|line| line.strip_prefix("default_toolchain = ")).map(|it| it.trim_matches('"').to_string()));
        let mut candidates: Vec<PathBuf> = named.iter().map(|it| toolchains.join(it)).collect();
        if let Ok(entries) = std::fs::read_dir(&toolchains) {
            candidates.extend(entries.flatten().map(|it| it.path()).filter(|it| it.file_name().unwrap_or_default().to_string_lossy().starts_with("stable-")));
        }
        let toolchain = candidates.into_iter().find(|it| it.join("bin/cargo").is_file())?;

        let caches = ["registry", "git"].iter().map(|it| cargo.join(it)).filter(|it| it.is_dir()).collect();
        Some(Rust { toolchain, caches })
    }
}

fn installs_folder_of(bin_path: &str) -> Option<PathBuf> {
    let marker = "/mise/installs/";
    let end = bin_path.find(marker)? + marker.len() - 1;
    Some(Path::new(&bin_path[..end]).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tools_under_the_installs_folder_are_offered() {
        let listed = "/home/someone/.local/share/cargo/bin\n/home/someone/.local/share/mise/installs/ruby/3.4.7/bin\n/home/someone/.local/share/mise/installs/node/26/bin\n";

        let toolchains = Toolchains::listed(listed);
        assert_eq!(
            toolchains.installs.as_deref(),
            Some(Path::new("/home/someone/.local/share/mise/installs"))
        );
        assert_eq!(
            toolchains.path(),
            "/home/someone/.local/share/mise/installs/ruby/3.4.7/bin:/home/someone/.local/share/mise/installs/node/26/bin:/usr/bin"
        );

        assert_eq!(Toolchains::listed("/somewhere/else/bin\n"), Toolchains::default());
        assert_eq!(Toolchains::default().path(), "/usr/bin");
    }

    #[test]
    fn rust_is_the_default_toolchain_when_installed_and_stable_otherwise() {
        let root = std::env::temp_dir().join(format!("anna-rust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let rustup = root.join("rustup");
        let cargo = root.join("cargo");
        std::fs::create_dir_all(rustup.join("toolchains/stable-x86_64/bin")).unwrap();
        std::fs::write(rustup.join("toolchains/stable-x86_64/bin/cargo"), "").unwrap();
        std::fs::create_dir_all(cargo.join("registry")).unwrap();
        std::fs::write(cargo.join("credentials.toml"), "token").unwrap();

        std::fs::write(rustup.join("settings.toml"), "default_toolchain = \"1.97.1-x86_64\"\n").unwrap();
        let rust = Rust::in_folders(&rustup, &cargo).unwrap();
        assert_eq!(rust.toolchain, rustup.join("toolchains/stable-x86_64"), "the named default isn't installed");
        assert_eq!(rust.caches, [cargo.join("registry")], "git isn't there; credentials never are");

        std::fs::create_dir_all(rustup.join("toolchains/1.97.1-x86_64/bin")).unwrap();
        std::fs::write(rustup.join("toolchains/1.97.1-x86_64/bin/cargo"), "").unwrap();
        assert_eq!(Rust::in_folders(&rustup, &cargo).unwrap().toolchain, rustup.join("toolchains/1.97.1-x86_64"));

        let toolchains = Toolchains { installs: None, bins: Vec::new(), rust: Rust::in_folders(&rustup, &cargo) };
        assert_eq!(toolchains.path(), format!("{}:/usr/bin", rustup.join("toolchains/1.97.1-x86_64/bin").display()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
