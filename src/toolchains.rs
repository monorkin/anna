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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Toolchains {
    pub installs: Option<PathBuf>,
    pub bins: Vec<PathBuf>,
}

impl Toolchains {
    pub fn discover() -> Toolchains {
        let Some(mise) = paths::program("mise") else {
            return Toolchains::default();
        };
        let Some(home) = dirs::home_dir() else {
            return Toolchains::default();
        };

        match Command::new(mise).arg("bin-paths").current_dir(&home).output() {
            Ok(output) => Toolchains::listed(&String::from_utf8_lossy(&output.stdout)),
            Err(_) => Toolchains::default(),
        }
    }

    /// The PATH inside the sandbox: the tools first, then the system's.
    pub fn path(&self) -> String {
        self.bins
            .iter()
            .map(|it| it.to_string_lossy().into_owned())
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
            },
            None => Toolchains::default(),
        }
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
}
