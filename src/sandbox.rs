//! The sandbox a Claude Code session runs in when it touches a project.
//!
//! Bubblewrap with every namespace unshared. The session sees a read-only
//! /usr, the project, its own profile, and the proxy's socket — no home
//! directory, no network. The project is where it is outside: git keeps
//! absolute paths, and a worktree made on either side has to work on both. socat turns the socket into the localhost
//! proxy Claude Code is pointed at, so the Claude API is the only thing it
//! can reach. Permissions are skipped inside, because the sandbox is the
//! permission system.
//!
//! A hand gets the project writable, except for the parts of .git that run
//! code: the brain isn't sandboxed and will run git in that folder later, and
//! a hook or an fsmonitor command planted by a hand would run as the brain.
//! A reviewer gets the whole project read-only.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::Models;
use crate::deadline;
use crate::paths;
use crate::toolchains::{Mbx, Toolchains};

/// The shell that runs inside: socat turns the proxy's socket into the
/// localhost proxy Claude Code is pointed at, does the same for every
/// service the session was granted, `cargo` becomes mbx where there is one,
/// and it hands over to claude.
fn inside(services: &[Service], mbx: Option<&Mbx>) -> String {
    let mut script = String::from("\nsocat TCP-LISTEN:3128,fork,bind=127.0.0.1 UNIX-CONNECT:/run/proxy.sock &\n");
    for (n, service) in services.iter().enumerate() {
        script.push_str(&format!("socat TCP-LISTEN:{},fork,bind=127.0.0.1 UNIX-CONNECT:/run/service-{n}.sock &\n", service.port));
    }
    if let Some(mbx) = mbx {
        script.push_str(&format!(
            "mkdir -p {MBX_SHIM_INSIDE}\ncat > {MBX_SHIM_INSIDE}/cargo <<'SHIM'\n#!/bin/sh\nMBX_CARGO_SHIM_MODE=1 MBX_CARGO_SHIM_PATH={MBX_SHIM_INSIDE}/cargo exec {} \"$@\"\nSHIM\nchmod +x {MBX_SHIM_INSIDE}/cargo\n",
            mbx.binary.display()
        ));
    }
    script.push_str("sleep 0.3\nexec /opt/claude \"$@\"\n");
    script
}

pub const BROKER_INSIDE: &str = "/run/broker.sock";

/// Claude Code's switches for a session in here: workflows fan out into
/// more sessions, and the nonessential traffic is telemetry the proxy
/// refuses anyway. Background shells stay: a hand can start the test suite
/// and keep working while it runs.
const DOES_ITS_OWN_WORK: [&str; 2] = ["CLAUDE_CODE_DISABLE_WORKFLOWS", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"];
/// Told the package managers themselves, so each fails at once with its own
/// message instead of retrying against a proxy that refuses everything: a
/// day of hands spent minutes at a time asking mise and yarn for the network.
const WORKS_OFFLINE: [(&str, &str); 5] = [
    ("CARGO_NET_OFFLINE", "true"),
    ("MISE_OFFLINE", "1"),
    ("YARN_ENABLE_OFFLINE_MODE", "1"),
    ("npm_config_offline", "true"),
    ("BUNDLE_FROZEN", "true"),
];
/// Where a session builds when mbx doesn't keep its target folder. Never the
/// project's own target folder: what a session builds shouldn't land where
/// the person's own builds are picked up from.
const BUILD_INSIDE: &str = "/build";
const CARGO_INSIDE: &str = "/home/hand/.cargo";
/// Where `cargo` is mbx, first on the PATH. In the session's own /tmp: it is
/// written as the session starts, and goes with it.
const MBX_SHIM_INSIDE: &str = "/tmp/mbx";

/// A port on the host's loopback that a session may reach as the same port
/// on its own: a database for the tests, say. On the host, socat listens on
/// `socket` and connects to the port; inside, the socket is bound in and
/// socat listens on the port. The bridge dies with the session.
#[derive(Debug, Clone)]
pub struct Service {
    pub port: u16,
    pub socket: PathBuf,
}
const GIT_PARTS_THAT_RUN_CODE: [&str; 3] = [".git/config", ".git/hooks", ".git/modules"];

/// What every sandbox of this process is given from outside: the way to
/// Claude, and the tools it can run.
pub struct Outside {
    pub proxy_socket: PathBuf,
    pub toolchains: Toolchains,
    pub time_limit: Duration,
    /// Her git identity, when she has one of her own: what a hand's commits
    /// are authored as. It carries no credential; a hand can't push.
    pub gitconfig: Option<PathBuf>,
    /// Whether the user's systemd can be asked for a scope. With one, a
    /// session gets a ceiling on memory and on how many processes it may
    /// have, so a hand that forks without end or eats all the memory takes
    /// itself down and not the machine Anna runs on.
    pub scopes: bool,
    pub models: Models,
}

/// Shares of the machine's memory, which systemd takes as percentages. A
/// flat 8G was too little: cargo builds with one rustc per core, and on 32
/// cores a test build went past it and the kernel killed the reviewer
/// mid-review. Past the first share a session is slowed down, past the
/// second it is stopped. It may swap as much again: without swap, one slowed
/// down past its share has nowhere to put what it needs, the pressure that
/// builds has systemd-oomd kill it long before its ceiling: a fat-LTO link
/// of a large workspace was killed that way twice at under 12G of 29.
const SLOWED_PAST: &str = "MemoryHigh=40%";
const MOST_MEMORY: &str = "MemoryMax=50%";
const MOST_SWAP: &str = "MemorySwapMax=50%";
const MOST_PROCESSES: &str = "TasksMax=2048";
const SECONDS_TO_FIND_OUT: u64 = 10;

impl Outside {
    /// Asked with the very limits a session will get, so a systemd that
    /// gives scopes but can't set these is found out here and not by the
    /// first hand.
    pub fn can_have_scopes() -> bool {
        let mut trial = scope();
        trial.arg("/usr/bin/true");
        deadline::output_within(&mut trial, Duration::from_secs(SECONDS_TO_FIND_OUT)).is_some_and(|it| it.status.success())
    }
}

/// Where a project's builds are kept between sessions: under her data, by a
/// hash of the project's path, made on the way in.
pub fn build_dir_of(project: &Path) -> std::io::Result<PathBuf> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in project.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let directory = paths::data_dir().join("builds").join(format!("{hash:016x}"));
    paths::make_private_dir(&directory)?;
    Ok(directory)
}

/// A session's /tmp and home, made in its own folder on disk and gone with
/// it.
pub fn make_scratch(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory.join("tmp"))?;
    std::fs::create_dir_all(directory.join("home"))
}

/// How many things a session's cargo compiles at once: a quarter of the
/// cores, and at least two. Cargo would take every core, and two hands and a
/// reviewer each doing that on 32 cores put 24 GB of rustc in memory at
/// once and had the kernel kill one of them mid-build.
fn build_jobs() -> usize {
    std::thread::available_parallelism().map(|it| it.get()).unwrap_or(4).div_ceil(4).max(2)
}

/// `systemd-run`, up to where the program to run goes.
fn scope() -> Command {
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args(["-p", SLOWED_PAST, "-p", MOST_MEMORY, "-p", MOST_SWAP, "-p", MOST_PROCESSES])
        .arg("--");
    command
}

pub struct Sandbox<'outside> {
    pub project: PathBuf,
    pub profile: PathBuf,
    pub outside: &'outside Outside,
    pub broker_socket: Option<PathBuf>,
    pub writable: bool,
    /// Where builds of this project go, on the host: kept between hands so
    /// the second one doesn't compile the world again.
    pub build_dir: PathBuf,
    pub services: Vec<Service>,
    /// A proxy of this session's own that also lets it at the package
    /// registries. With one, the package managers aren't told they are
    /// offline, because they aren't.
    pub registries: Option<PathBuf>,
    /// Where the session's /tmp and home live, on disk. A tmpfs there is
    /// memory: one reviewer building into /tmp held 17 GB of it, and the
    /// machine ran out.
    pub scratch: PathBuf,
}

impl Sandbox<'_> {
    /// `claude` inside the sandbox; whatever arguments the caller adds go to
    /// claude.
    pub fn claude(&self, claude_binary: &Path) -> Command {
        let mut command = self.contained();
        command
            // Nothing of Anna's environment goes in: whatever tokens she was
            // started with are hers, and what a session needs is set below
            .args(["--unshare-all", "--die-with-parent", "--clearenv"])
            .args(["--ro-bind", "/usr", "/usr"])
            .args(["--symlink", "usr/bin", "/bin"])
            .args(["--symlink", "usr/lib", "/lib"])
            .args(["--symlink", "usr/lib", "/lib64"])
            .args(["--proc", "/proc", "--dev", "/dev"])
            .arg("--bind")
            .arg(self.scratch.join("tmp"))
            .arg("/tmp")
            .arg("--bind")
            .arg(self.scratch.join("home"))
            .arg("/home/hand");
        if let Some(gitconfig) = &self.outside.gitconfig {
            command.arg("--ro-bind").arg(gitconfig).arg("/home/hand/.gitconfig");
        }

        for certificates in ["/etc/ssl", "/etc/ca-certificates", "/etc/pki"] {
            if Path::new(certificates).exists() {
                command.args(["--ro-bind", certificates, certificates]);
            }
        }

        self.bind_project(&mut command);
        self.bind_toolchains(&mut command);
        command
            .arg("--ro-bind")
            .arg(claude_binary)
            .arg("/opt/claude")
            .arg("--bind")
            .arg(&self.profile)
            .arg("/profile")
            .arg("--ro-bind")
            .arg(self.registries.as_ref().unwrap_or(&self.outside.proxy_socket))
            .arg("/run/proxy.sock");
        if !self.mbx_keeps_the_target() {
            command.arg("--bind").arg(&self.build_dir).arg(BUILD_INSIDE);
            command.args(["--setenv", "CARGO_TARGET_DIR", BUILD_INSIDE]);
        }
        if let Some(broker_socket) = &self.broker_socket {
            command.arg("--ro-bind").arg(broker_socket).arg(BROKER_INSIDE);
        }
        for (n, service) in self.services.iter().enumerate() {
            command.arg("--ro-bind").arg(&service.socket).arg(format!("/run/service-{n}.sock"));
        }

        command
            .arg("--chdir")
            .arg(&self.project)
            .args(["--setenv", "HOME", "/home/hand"])
            .args(["--setenv", "LANG", "C.UTF-8"])
            .args(["--setenv", "PATH", &self.path()])
            .args(["--setenv", "CLAUDE_CONFIG_DIR", "/profile"])
            .args(["--setenv", "HTTPS_PROXY", "http://127.0.0.1:3128"])
            .args(["--setenv", "CARGO_HOME", CARGO_INSIDE])
            .args(["--setenv", "CARGO_BUILD_JOBS", &build_jobs().to_string()])
            .args(["--setenv", "GIT_TERMINAL_PROMPT", "0"])
            .args(["--setenv", "MISE_TRUSTED_CONFIG_PATHS", &self.trusted_by_mise()]);
        if self.registries.is_none() {
            command.args(WORKS_OFFLINE.iter().flat_map(|(name, value)| ["--setenv", name, value]));
        }
        command
            .args(DOES_ITS_OWN_WORK.iter().flat_map(|name| ["--setenv", name, "1"]))
            .args(["/usr/bin/bash", "-c", &inside(&self.services, self.outside.toolchains.mbx.as_ref()), "sandbox"]);
        command
    }

    /// mbx keeps a session's target folder in her store, behind a `target`
    /// link in the project, and prunes it to a budget. Only where it can: a
    /// reviewer can't write that link, so it uses one only when the hand left
    /// it there; and a target that is a plain folder is the person's own
    /// build, which no session builds into.
    fn mbx_keeps_the_target(&self) -> bool {
        let Some(mbx) = &self.outside.toolchains.mbx else {
            return false;
        };
        let target = self.project.join("target");
        match std::fs::read_link(&target) {
            Ok(link) => self.writable || link.starts_with(&mbx.store),
            Err(_) => self.writable && !target.exists(),
        }
    }

    fn path(&self) -> String {
        match &self.outside.toolchains.mbx {
            Some(_) => format!("{MBX_SHIM_INSIDE}:{}", self.outside.toolchains.path()),
            None => self.outside.toolchains.path(),
        }
    }

    /// Claude Code's own tools for a session in here, for its `--tools`:
    /// these and only these. No sub-agents, no workflows, no scheduling — a
    /// session in here is one worker doing one job, and every agent it
    /// started would be another full session spending the same allowance.
    /// Background shells and the tools that read and stop them are in:
    /// waiting on a test suite while doing something else is still one
    /// worker. (`Monitor` isn't: Claude Code drops it from a `--tools` list.)
    /// A reviewer can look and run things, not change them.
    pub fn tools(&self) -> &'static str {
        if self.writable {
            "Bash,Read,Edit,Write,Glob,Grep,Skill,ToolSearch,TaskOutput,TaskStop"
        } else {
            "Bash,Read,Glob,Grep,Skill,ToolSearch,TaskOutput,TaskStop"
        }
    }

    /// mise's installs read-only, and Rust: the toolchain read-only, and
    /// cargo's caches with a writable layer on top, because cargo unpacks
    /// crates into its cache as it builds and would refuse a read-only one.
    /// The layer is thrown away with the sandbox; the caches on the host are
    /// never written.
    fn bind_toolchains(&self, command: &mut Command) {
        if let Some(installs) = &self.outside.toolchains.installs {
            command.arg("--ro-bind").arg(installs).arg(installs);
        }
        if let Some(rust) = &self.outside.toolchains.rust {
            command.arg("--ro-bind").arg(&rust.toolchain).arg(&rust.toolchain);
            for cache in &rust.caches {
                let name = cache.file_name().unwrap_or_default().to_string_lossy();
                command.arg("--overlay-src").arg(cache).arg("--tmp-overlay").arg(format!("{CARGO_INSIDE}/{name}"));
            }
        }
        // Where it is outside, because the `target` link mbx leaves in the
        // project has to lead somewhere on both sides
        if let Some(mbx) = &self.outside.toolchains.mbx {
            command.arg("--bind").arg(&mbx.store).arg(&mbx.store);
            command.arg("--setenv").arg("MBX_CACHE_DIR").arg(&mbx.store);
        }
    }

    /// bwrap, inside a scope of its own when there can be one. A scope runs
    /// the program in place of systemd-run, so the process Anna started is
    /// still the one she watches and stops.
    fn contained(&self) -> Command {
        if self.outside.scopes {
            let mut command = scope();
            command.arg("bwrap");
            command
        } else {
            Command::new("bwrap")
        }
    }

    fn bind_project(&self, command: &mut Command) {
        let git = self.project.join(".git");
        let holding = repository_holding(&self.project);
        if self.writable {
            command.arg("--bind").arg(&self.project).arg(&self.project);
            if git.join("HEAD").is_file() {
                lock_git(command, &self.project);
            } else if git.is_file() {
                // A worktree's .git is a file that says where the real one
                // is; rewritten, it would point the brain's git anywhere
                command.arg("--ro-bind").arg(&git).arg(&git);
                if let Some(repository) = &holding {
                    lock_git(command, repository);
                }
            } else {
                // Starting a repository is the brain's to do. A hand that
                // could would write the hooks and config of one from scratch
                command.arg("--tmpfs").arg(&git).arg("--remount-ro").arg(&git);
            }
        } else {
            command.arg("--ro-bind").arg(&self.project).arg(&self.project);
            if let Some(repository) = &holding {
                let git = repository.join(".git");
                command.arg("--ro-bind").arg(&git).arg(&git);
            }
        }
    }

    /// The person's `mise trust` lives in a home the sandbox doesn't have,
    /// and a mise that doesn't trust the project's config refuses to start
    /// anything at all. A session runs the project's code either way, so
    /// its config is trusted in here — and in a worktree, the repository's
    /// too, because mise reads the configs of the folders above it.
    fn trusted_by_mise(&self) -> String {
        let mut trusted = vec![self.project.clone()];
        trusted.extend(repository_holding(&self.project));
        trusted.iter().map(|it| it.display().to_string()).collect::<Vec<_>>().join(":")
    }
}

/// .git becomes its own mount so it can't be renamed out of the way and
/// replaced, and the parts of it that run code become read-only. A part
/// that doesn't exist yet is covered with an empty read-only tmpfs so it
/// can't be created either.
fn lock_git(command: &mut Command, repository: &Path) {
    let git = repository.join(".git");
    command.arg("--bind").arg(&git).arg(&git);

    for part in GIT_PARTS_THAT_RUN_CODE {
        let part = repository.join(part);
        if part.exists() {
            command.arg("--ro-bind").arg(&part).arg(&part);
        } else {
            command.arg("--tmpfs").arg(&part).arg("--remount-ro").arg(&part);
        }
    }
}

/// The repository a linked worktree keeps its git in, when that repository
/// holds the worktree — the .claude/worktrees layout — so a session in the
/// worktree can use git. Only the nearest repository above it counts, and
/// only when the worktree's .git file points into that repository's own
/// worktrees: that file sits where a hand working in the repository could
/// have rewritten it, and it must not bring in anything else.
fn repository_holding(project: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(project.join(".git")).ok()?;
    let gitdir = project.join(pointer.strip_prefix("gitdir:")?.trim()).canonicalize().ok()?;
    let repository = project.ancestors().skip(1).find(|it| it.join(".git/HEAD").is_file())?;
    let worktrees = repository.join(".git/worktrees").canonicalize().ok()?;
    if gitdir.starts_with(&worktrees) {
        Some(repository.to_path_buf())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn arguments_of(sandbox: &Sandbox) -> Vec<String> {
        sandbox
            .claude(Path::new("/bin/claude"))
            .get_args()
            .map(|it| it.to_string_lossy().into_owned())
            .collect()
    }

    fn binds<'a>(arguments: &'a [String], flag: &str) -> Vec<(&'a str, &'a str)> {
        arguments
            .windows(3)
            .filter(|it| it[0] == flag)
            .map(|it| (it[1].as_str(), it[2].as_str()))
            .collect()
    }

    #[test]
    fn a_hand_can_write_its_project_but_not_the_parts_of_git_that_run_code() {
        let project = std::env::temp_dir().join(format!("anna-sandbox-{}", std::process::id()));
        fs::create_dir_all(project.join(".git/hooks")).unwrap();
        fs::write(project.join(".git/config"), "").unwrap();
        fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let project_path = project.to_str().unwrap();

        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            gitconfig: None,
            scopes: false,
            models: Models::default(),
        };
        let arguments = arguments_of(&Sandbox {
            project: project.clone(),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: vec![Service { port: 33380, socket: PathBuf::from("/run/anna/service-h1-0.sock") }],
            registries: None,
            scratch: PathBuf::from("/data/hands/h1"),
        });

        assert!(arguments.contains(&"--unshare-all".to_string()));
        let git = format!("{project_path}/.git");
        assert_eq!(
            binds(&arguments, "--bind"),
            [("/data/hands/h1/tmp", "/tmp"), ("/data/hands/h1/home", "/home/hand"), (project_path, project_path), (git.as_str(), git.as_str()), ("/data/hands/h1/profile", "/profile"), ("/data/builds/abc", BUILD_INSIDE)]
        );
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/service-h1-0.sock", "/run/service-0.sock")));
        assert!(arguments.last().unwrap() == "sandbox" && arguments[arguments.len() - 2].contains("TCP-LISTEN:33380,fork,bind=127.0.0.1 UNIX-CONNECT:/run/service-0.sock"));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CARGO_TARGET_DIR" && it[2] == BUILD_INSIDE));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CLAUDE_CODE_DISABLE_WORKFLOWS" && it[2] == "1"));
        for (name, value) in [("CARGO_NET_OFFLINE", "true"), ("MISE_OFFLINE", "1"), ("YARN_ENABLE_OFFLINE_MODE", "1"), ("npm_config_offline", "true")] {
            assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == name && it[2] == value), "{name} tells its tool there is no network");
        }
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/proxy.sock", "/run/proxy.sock")));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CARGO_BUILD_JOBS" && it[2].parse::<usize>().is_ok_and(|jobs| jobs >= 2)), "cargo doesn't take every core");

        let with_registries = arguments_of(&Sandbox {
            project: project.clone(),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
            registries: Some(PathBuf::from("/run/anna/proxy-h1.sock")),
            scratch: PathBuf::from("/data/hands/h1"),
        });
        assert!(binds(&with_registries, "--ro-bind").contains(&("/run/anna/proxy-h1.sock", "/run/proxy.sock")), "its own proxy stands in for the shared one");
        assert!(!with_registries.iter().any(|it| it == "MISE_OFFLINE" || it == "CARGO_NET_OFFLINE"), "the package managers aren't told they are offline");
        assert!(with_registries.windows(3).any(|it| it[0] == "--setenv" && it[1] == "GIT_TERMINAL_PROMPT" && it[2] == "0"), "git still never asks for a login");
        assert!(!arguments.iter().any(|it| it == "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"), "a hand may run its tests in the background");
        let read_only = binds(&arguments, "--ro-bind");
        let config = format!("{git}/config");
        let hooks = format!("{git}/hooks");
        assert!(read_only.contains(&(config.as_str(), config.as_str())));
        assert!(read_only.contains(&(hooks.as_str(), hooks.as_str())));
        assert!(arguments.windows(2).any(|it| it[0] == "--remount-ro" && it[1] == format!("{git}/modules")));
        assert!(!arguments.iter().any(|it| it == "/home" || it == "/etc"));
        assert!(!arguments.iter().any(|it| it == BROKER_INSIDE));

        fs::remove_dir_all(project).unwrap();
    }

    /// The real thing, with a shell where Claude would be: what the session
    /// would find in its environment and be able to do to the project.
    #[test]
    fn a_hand_gets_none_of_annas_environment_and_cannot_start_or_repoint_a_repository() {
        if crate::paths::program("bwrap").is_none() || crate::paths::program("socat").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("anna-sandbox-real-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let plain = root.join("plain");
        let worktree = root.join("worktree");
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(root.join("profile")).unwrap();
        make_scratch(&root.join("scratch")).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /somewhere/real\n").unwrap();
        fs::write(root.join("proxy.sock"), "").unwrap();

        let outside = Outside {
            proxy_socket: root.join("proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            gitconfig: None,
            scopes: Outside::can_have_scopes(),
            models: Models::default(),
        };
        let build_dir = root.join("build");
        fs::create_dir_all(&build_dir).unwrap();
        let run = |project: &Path, script: &str| {
            let sandbox = Sandbox {
                project: project.to_path_buf(),
                profile: root.join("profile"),
                outside: &outside,
                broker_socket: None,
                writable: true,
                build_dir: build_dir.clone(),
                services: Vec::new(),
                registries: None,
                scratch: root.join("scratch"),
            };
            let output = sandbox.claude(Path::new("/usr/bin/bash")).args(["-c", script]).env("ANNA_TEST_SECRET", "s3cret").output().unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        let environment = run(&plain, "env");
        assert!(!environment.contains("ANNA_TEST_SECRET"), "{environment}");
        assert!(environment.contains("HOME=/home/hand"));

        let attempts = "touch made-it; (mkdir -p .git/hooks && echo 'evil' > .git/hooks/pre-commit) 2>/dev/null && echo started; echo 'gitdir: /evil' > .git 2>/dev/null && echo repointed; echo done";
        assert_eq!(run(&plain, attempts).trim(), "done");
        assert!(plain.join("made-it").exists(), "the project itself is still writable");
        assert_eq!(run(&worktree, attempts).trim(), "done");
        assert_eq!(fs::read_to_string(worktree.join(".git")).unwrap(), "gitdir: /somewhere/real\n");
        assert_eq!(run(&plain, "touch /build/made-here && echo built").trim(), "built");
        assert!(build_dir.join("made-here").exists(), "builds land in the folder given for them");

        fs::remove_dir_all(root).unwrap();
    }

    /// git keeps absolute paths: a worktree the person made works for a
    /// hand, and one a hand made works for the person.
    #[test]
    fn worktrees_work_on_both_sides_of_the_sandbox() {
        if crate::paths::program("bwrap").is_none() || crate::paths::program("socat").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("anna-sandbox-git-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("profile")).unwrap();
        make_scratch(&root.join("scratch")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("proxy.sock"), "").unwrap();
        let git = |arguments: &[&str]| {
            let output = Command::new("git").arg("-C").arg(&project).args(arguments).output().unwrap();
            assert!(output.status.success(), "git {arguments:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Someone"]);
        git(&["config", "user.email", "someone@example.com"]);
        git(&["commit", "-q", "--allow-empty", "-m", "Start"]);
        git(&["worktree", "add", "-q", ".claude/worktrees/outside", "-b", "outside"]);

        let outside = Outside {
            proxy_socket: root.join("proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            gitconfig: None,
            scopes: Outside::can_have_scopes(),
            models: Models::default(),
        };
        let run = |project: &Path, script: &str| {
            let sandbox = Sandbox {
                project: project.to_path_buf(),
                profile: root.join("profile"),
                outside: &outside,
                broker_socket: None,
                writable: true,
                build_dir: root.join("build"),
                services: Vec::new(),
                registries: None,
                scratch: root.join("scratch"),
            };
            let output = sandbox.claude(Path::new("/usr/bin/bash")).args(["-c", script]).output().unwrap();
            format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        };

        let said = run(&project, "git -C .claude/worktrees/outside commit -q --allow-empty -m 'Made inside' && git worktree add -q .claude/worktrees/inside -b inside && echo done");
        assert_eq!(said.trim(), "done");
        assert_eq!(git(&["log", "-1", "--format=%s", "outside"]).trim(), "Made inside");
        let inside = project.join(".claude/worktrees/inside");
        let status = Command::new("git").arg("-C").arg(&inside).args(["status", "--short", "--branch"]).output().unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).starts_with("## inside"), "{}", String::from_utf8_lossy(&status.stderr));

        let said = run(&project.join(".claude/worktrees/outside"), "git commit -q --allow-empty -m 'Made in the worktree' && echo done; (echo evil > ../../../.git/hooks/pre-commit) 2>/dev/null || echo refused");
        assert_eq!(said.lines().collect::<Vec<_>>(), ["done", "refused"], "{said}");
        assert_eq!(git(&["log", "-1", "--format=%s", "outside"]).trim(), "Made in the worktree");

        let worktree = project.join(".claude/worktrees/outside");
        let said = run(&worktree, "echo $MISE_TRUSTED_CONFIG_PATHS");
        assert_eq!(said.trim(), format!("{}:{}", worktree.display(), project.display()), "mise trusts the worktree and the repository above it");

        // A .git file a hand rewrote to point at some other repository
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        Command::new("git").arg("-C").arg(&elsewhere).args(["init", "-q"]).output().unwrap();
        fs::create_dir_all(elsewhere.join(".git/worktrees/stolen")).unwrap();
        let planted = project.join(".claude/worktrees/planted");
        fs::create_dir_all(&planted).unwrap();
        fs::write(planted.join(".git"), format!("gitdir: {}\n", elsewhere.join(".git/worktrees/stolen").display())).unwrap();
        let said = run(&planted, &format!("ls {} 2>/dev/null || echo unseen", elsewhere.join(".git").display()));
        assert_eq!(said.trim(), "unseen");
        fs::remove_dir_all(root).unwrap();
    }

    /// A hand with Rust and a bridged service, for real: a crate with a
    /// dependency builds offline from the host's cache, and a listener on
    /// the host's loopback answers on the sandbox's.
    #[test]
    fn a_hand_builds_offline_and_reaches_a_service_it_was_granted() {
        if crate::paths::program("bwrap").is_none() || crate::paths::program("socat").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("anna-sandbox-rust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut toolchains = Toolchains::discover();
        let Some(rust) = toolchains.rust.clone() else {
            return;
        };
        // mbx where this machine has it, with a store of the test's own
        toolchains.mbx = toolchains.mbx.take().map(|mbx| Mbx { store: root.join("mbx"), ..mbx });
        fs::create_dir_all(root.join("mbx")).unwrap();
        let project = root.join("crate");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(root.join("profile")).unwrap();
        make_scratch(&root.join("scratch")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("proxy.sock"), "").unwrap();
        // A dependency that is in the host's cache because Anna herself uses it
        fs::write(project.join("Cargo.toml"), "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nlibc = \"0.2\"\n").unwrap();
        fs::write(project.join("src/main.rs"), "fn main() { println!(\"pid {}\", unsafe { libc::getpid() } > 0); }\n").unwrap();
        let fetched = Command::new(rust.toolchain.join("bin/cargo")).args(["generate-lockfile", "--offline"]).current_dir(&project).output().unwrap();
        assert!(fetched.status.success(), "{}", String::from_utf8_lossy(&fetched.stderr));

        // Something on the host's loopback to reach: a shell answering one line
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                let _ = std::io::Write::write_all(&mut connection, b"hello from the host\n");
            }
        });
        let socket = root.join("service.sock");
        let mut bridge = Command::new("socat")
            .arg(format!("UNIX-LISTEN:{},fork", socket.display()))
            .arg(format!("TCP:127.0.0.1:{port}"))
            .spawn()
            .unwrap();
        while !socket.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }

        let outside = Outside { proxy_socket: root.join("proxy.sock"), toolchains, time_limit: Duration::from_secs(60), gitconfig: None, scopes: Outside::can_have_scopes(), models: Models::default() };
        let sandbox = Sandbox {
            project: project.clone(),
            profile: root.join("profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
            build_dir: root.join("build"),
            services: vec![Service { port, socket: socket.clone() }],
            registries: None,
            scratch: root.join("scratch"),
        };
        let script = format!("cargo run -q 2>&1; echo | socat - TCP:127.0.0.1:{port}; cat ~/.cargo/credentials.toml 2>/dev/null && echo LEAKED");
        let output = sandbox.claude(Path::new("/usr/bin/bash")).args(["-c", &script]).output().unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        let _ = bridge.kill();
        let _ = bridge.wait();

        assert!(said.contains("pid true"), "the crate built and ran offline:\n{said}\n{}", String::from_utf8_lossy(&output.stderr));
        assert!(said.contains("hello from the host"), "and the service answered:\n{said}");
        assert!(!said.contains("LEAKED"));
        match &outside.toolchains.mbx {
            Some(mbx) => {
                let target = fs::read_link(project.join("target")).expect("mbx keeps the target, behind a link");
                assert!(target.starts_with(&mbx.store), "in her store: {}", target.display());
                assert!(target.join("debug").exists());
            }
            None => assert!(root.join("build/debug").exists(), "built into /build"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_session_in_the_sandbox_gets_its_own_tools_and_no_agents_of_its_own() {
        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            gitconfig: None,
            scopes: false,
            models: Models::default(),
        };
        let sandbox = |writable| Sandbox {
            project: PathBuf::from("/srv/project"),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
            registries: None,
            scratch: PathBuf::from("/data/hands/h1"),
        };

        let hand: Vec<&str> = sandbox(true).tools().split(',').collect();
        let reviewer: Vec<&str> = sandbox(false).tools().split(',').collect();
        assert!(hand.contains(&"Edit") && hand.contains(&"Bash"));
        assert!(!reviewer.contains(&"Edit") && !reviewer.contains(&"Write"), "a reviewer changes nothing");
        assert!(hand.contains(&"TaskOutput") && hand.contains(&"TaskStop"), "reading its own background work is fine");
        for spends_more in ["Task", "Agent", "Workflow", "SendMessage", "RemoteTrigger", "CronCreate", "ScheduleWakeup"] {
            assert!(!hand.contains(&spends_more) && !reviewer.contains(&spends_more), "{spends_more} would start more sessions");
        }
    }

    #[test]
    fn a_reviewer_can_write_nothing_in_the_project() {
        let installs = PathBuf::from("/home/someone/.local/share/mise/installs");
        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains {
                installs: Some(installs.clone()),
                bins: vec![installs.join("ruby/3.4.7/bin")],
                rust: None,
                mbx: None,
            },
            time_limit: Duration::from_secs(60),
            gitconfig: Some(PathBuf::from("/home/someone/.config/anna/tools/gitconfig")),
            scopes: true,
            models: Models::default(),
        };
        let arguments = arguments_of(&Sandbox {
            project: PathBuf::from("/home/someone/project"),
            profile: PathBuf::from("/data/reviews/r1/profile"),
            outside: &outside,
            broker_socket: Some(PathBuf::from("/run/anna/hand-h1.sock")),
            writable: false,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
            registries: None,
            scratch: PathBuf::from("/data/hands/h1"),
        });

        let bwrap = arguments.iter().position(|it| it == "bwrap").expect("with a scope to be had, bwrap runs inside one");
        assert!(arguments[..bwrap].contains(&"--scope".to_string()));
        assert!(arguments[..bwrap].iter().any(|it| it.starts_with("MemoryMax=")));
        assert!(arguments[..bwrap].contains(&"MemorySwapMax=50%".to_string()), "a session may swap rather than be killed under pressure");
        assert!(arguments[..bwrap].iter().any(|it| it.starts_with("TasksMax=")));

        assert_eq!(binds(&arguments, "--bind"), [("/data/hands/h1/tmp", "/tmp"), ("/data/hands/h1/home", "/home/hand"), ("/data/reviews/r1/profile", "/profile"), ("/data/builds/abc", BUILD_INSIDE)]);
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CARGO_TARGET_DIR" && it[2] == BUILD_INSIDE), "without mbx, builds go to a folder of their own");
        assert!(binds(&arguments, "--ro-bind").contains(&(installs.to_str().unwrap(), installs.to_str().unwrap())));
        assert!(binds(&arguments, "--ro-bind").contains(&("/home/someone/.config/anna/tools/gitconfig", "/home/hand/.gitconfig")), "her commits are authored as her");
        assert!(arguments.windows(3).any(|it| {
            it[0] == "--setenv" && it[1] == "PATH" && it[2] == "/home/someone/.local/share/mise/installs/ruby/3.4.7/bin:/usr/bin"
        }));
        assert!(binds(&arguments, "--ro-bind").contains(&("/home/someone/project", "/home/someone/project")));
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/hand-h1.sock", BROKER_INSIDE)));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "MISE_TRUSTED_CONFIG_PATHS" && it[2] == "/home/someone/project"));
    }

    #[test]
    fn with_mbx_cargo_is_mbx_and_it_keeps_every_target_it_may() {
        let root = std::env::temp_dir().join(format!("anna-sandbox-mbx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        fs::create_dir_all(&store).unwrap();
        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains { mbx: Some(Mbx { binary: PathBuf::from("/installs/mr-boxington/1.10.0/mbx"), store: store.clone() }), ..Toolchains::default() },
            time_limit: Duration::from_secs(60),
            gitconfig: None,
            scopes: false,
            models: Models::default(),
        };
        let sandbox = |project: &str, writable| Sandbox {
            project: root.join(project),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
            registries: None,
            scratch: PathBuf::from("/data/hands/h1"),
        };
        for project in ["fresh", "linked", "own-build"] {
            fs::create_dir_all(root.join(project)).unwrap();
        }
        std::os::unix::fs::symlink(store.join("targets/v1/abc"), root.join("linked/target")).unwrap();
        fs::create_dir_all(root.join("own-build/target")).unwrap();

        let arguments = arguments_of(&sandbox("fresh", true));
        let store_path = store.to_str().unwrap();
        assert!(binds(&arguments, "--bind").contains(&(store_path, store_path)), "the store is where it is outside, writable");
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "MBX_CACHE_DIR" && it[2] == store_path));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "PATH" && it[2].starts_with("/tmp/mbx:")));
        let script = arguments.iter().find(|it| it.contains("cat > /tmp/mbx/cargo")).expect("the shim is written as the session starts");
        assert!(script.contains("MBX_CARGO_SHIM_MODE=1 MBX_CARGO_SHIM_PATH=/tmp/mbx/cargo exec /installs/mr-boxington/1.10.0/mbx \"$@\""));

        let builds_in_its_own_folder = |arguments: &[String]| arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CARGO_TARGET_DIR");
        assert!(!builds_in_its_own_folder(&arguments), "a hand in a fresh worktree builds where mbx keeps it");
        assert!(!builds_in_its_own_folder(&arguments_of(&sandbox("linked", false))), "a reviewer builds where the hand's link leads");
        assert!(builds_in_its_own_folder(&arguments_of(&sandbox("fresh", false))), "a reviewer can't make the link");
        assert!(builds_in_its_own_folder(&arguments_of(&sandbox("own-build", true))), "the person's own target folder is never built into");
        fs::remove_dir_all(root).unwrap();
    }
}
