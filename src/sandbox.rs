//! The sandbox a Claude Code session runs in when it touches a project.
//!
//! Bubblewrap with every namespace unshared. The session sees a read-only
//! /usr, the project at /work, its own profile, and the proxy's socket — no
//! home directory, no network. socat turns the socket into the localhost
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

use crate::deadline;
use crate::paths;
use crate::toolchains::Toolchains;

/// The shell that runs inside: socat turns the proxy's socket into the
/// localhost proxy Claude Code is pointed at, does the same for every
/// service the session was granted, and hands over to claude.
fn inside(services: &[Service]) -> String {
    let mut script = String::from("\nsocat TCP-LISTEN:3128,fork,bind=127.0.0.1 UNIX-CONNECT:/run/proxy.sock &\n");
    for (n, service) in services.iter().enumerate() {
        script.push_str(&format!("socat TCP-LISTEN:{},fork,bind=127.0.0.1 UNIX-CONNECT:/run/service-{n}.sock &\n", service.port));
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
/// Where a session builds. Never the project's own target folder: that is
/// often a link into a cache the sandbox can't see, and what a hand builds
/// shouldn't land where the person's own builds are picked up from.
const BUILD_INSIDE: &str = "/build";
const CARGO_INSIDE: &str = "/home/hand/.cargo";

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
    /// Whether the user's systemd can be asked for a scope. With one, a
    /// session gets a ceiling on memory and on how many processes it may
    /// have, so a hand that forks without end or eats all the memory takes
    /// itself down and not the machine Anna runs on.
    pub scopes: bool,
}

/// Shares of the machine's memory, which systemd takes as percentages. A
/// flat 8G was too little: cargo builds with one rustc per core, and on 32
/// cores a test build went past it and the kernel killed the reviewer
/// mid-review. Past the first share a session is slowed down, past the
/// second it is stopped.
const SLOWED_PAST: &str = "MemoryHigh=40%";
const MOST_MEMORY: &str = "MemoryMax=50%";
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

/// `systemd-run`, up to where the program to run goes.
fn scope() -> Command {
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args(["-p", SLOWED_PAST, "-p", MOST_MEMORY, "-p", "MemorySwapMax=0", "-p", MOST_PROCESSES])
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
            .args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/home/hand"]);

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
            .arg("--bind")
            .arg(&self.build_dir)
            .arg(BUILD_INSIDE)
            .arg("--ro-bind")
            .arg(&self.outside.proxy_socket)
            .arg("/run/proxy.sock");
        if let Some(broker_socket) = &self.broker_socket {
            command.arg("--ro-bind").arg(broker_socket).arg(BROKER_INSIDE);
        }
        for (n, service) in self.services.iter().enumerate() {
            command.arg("--ro-bind").arg(&service.socket).arg(format!("/run/service-{n}.sock"));
        }

        command
            .args(["--chdir", "/work"])
            .args(["--setenv", "HOME", "/home/hand"])
            .args(["--setenv", "LANG", "C.UTF-8"])
            .args(["--setenv", "PATH", &self.outside.toolchains.path()])
            .args(["--setenv", "CLAUDE_CONFIG_DIR", "/profile"])
            .args(["--setenv", "HTTPS_PROXY", "http://127.0.0.1:3128"])
            .args(["--setenv", "CARGO_TARGET_DIR", BUILD_INSIDE])
            .args(["--setenv", "CARGO_HOME", CARGO_INSIDE])
            .args(["--setenv", "CARGO_NET_OFFLINE", "true"])
            .args(DOES_ITS_OWN_WORK.iter().flat_map(|name| ["--setenv", name, "1"]))
            .args(["/usr/bin/bash", "-c", &inside(&self.services), "sandbox"]);
        command
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
            "Bash,Read,Edit,Write,Glob,Grep,ToolSearch,TaskOutput,TaskStop"
        } else {
            "Bash,Read,Glob,Grep,ToolSearch,TaskOutput,TaskStop"
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
        if self.writable {
            command.arg("--bind").arg(&self.project).arg("/work");
            let git = self.project.join(".git");
            if git.join("HEAD").is_file() {
                self.lock_git(command);
            } else if git.is_file() {
                // A worktree's .git is a file that says where the real one
                // is; rewritten, it would point the brain's git anywhere
                command.arg("--ro-bind").arg(&git).arg("/work/.git");
            } else {
                // Starting a repository is the brain's to do. A hand that
                // could would write the hooks and config of one from scratch
                command.args(["--tmpfs", "/work/.git", "--remount-ro", "/work/.git"]);
            }
        } else {
            command.arg("--ro-bind").arg(&self.project).arg("/work");
        }
    }

    /// .git becomes its own mount so it can't be renamed out of the way and
    /// replaced, and the parts of it that run code become read-only. A part
    /// that doesn't exist yet is covered with an empty read-only tmpfs so it
    /// can't be created either.
    fn lock_git(&self, command: &mut Command) {
        command.arg("--bind").arg(self.project.join(".git")).arg("/work/.git");

        for part in GIT_PARTS_THAT_RUN_CODE {
            let outside = self.project.join(part);
            let inside = Path::new("/work").join(part);
            if outside.exists() {
                command.arg("--ro-bind").arg(outside).arg(inside);
            } else {
                command.arg("--tmpfs").arg(&inside).arg("--remount-ro").arg(&inside);
            }
        }
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
            scopes: false,
        };
        let arguments = arguments_of(&Sandbox {
            project: project.clone(),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: vec![Service { port: 33380, socket: PathBuf::from("/run/anna/service-h1-0.sock") }],
        });

        assert!(arguments.contains(&"--unshare-all".to_string()));
        let git = format!("{project_path}/.git");
        assert_eq!(
            binds(&arguments, "--bind"),
            [(project_path, "/work"), (git.as_str(), "/work/.git"), ("/data/hands/h1/profile", "/profile"), ("/data/builds/abc", BUILD_INSIDE)]
        );
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/service-h1-0.sock", "/run/service-0.sock")));
        assert!(arguments.last().unwrap() == "sandbox" && arguments[arguments.len() - 2].contains("TCP-LISTEN:33380,fork,bind=127.0.0.1 UNIX-CONNECT:/run/service-0.sock"));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CARGO_TARGET_DIR" && it[2] == BUILD_INSIDE));
        assert!(arguments.windows(3).any(|it| it[0] == "--setenv" && it[1] == "CLAUDE_CODE_DISABLE_WORKFLOWS" && it[2] == "1"));
        assert!(!arguments.iter().any(|it| it == "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"), "a hand may run its tests in the background");
        let read_only = binds(&arguments, "--ro-bind");
        assert!(read_only.contains(&(format!("{git}/config").as_str(), "/work/.git/config")));
        assert!(read_only.contains(&(format!("{git}/hooks").as_str(), "/work/.git/hooks")));
        assert!(arguments.windows(2).any(|it| it[0] == "--remount-ro" && it[1] == "/work/.git/modules"));
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
        fs::write(worktree.join(".git"), "gitdir: /somewhere/real\n").unwrap();
        fs::write(root.join("proxy.sock"), "").unwrap();

        let outside = Outside {
            proxy_socket: root.join("proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            scopes: Outside::can_have_scopes(),
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

    /// A hand with Rust and a bridged service, for real: a crate with a
    /// dependency builds offline from the host's cache, and a listener on
    /// the host's loopback answers on the sandbox's.
    #[test]
    fn a_hand_builds_offline_and_reaches_a_service_it_was_granted() {
        if crate::paths::program("bwrap").is_none() || crate::paths::program("socat").is_none() {
            return;
        }
        let toolchains = Toolchains::discover();
        let Some(rust) = &toolchains.rust else {
            return;
        };
        let root = std::env::temp_dir().join(format!("anna-sandbox-rust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("crate");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(root.join("profile")).unwrap();
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

        let outside = Outside { proxy_socket: root.join("proxy.sock"), toolchains, time_limit: Duration::from_secs(60), scopes: Outside::can_have_scopes() };
        let sandbox = Sandbox {
            project: project.clone(),
            profile: root.join("profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
            build_dir: root.join("build"),
            services: vec![Service { port, socket: socket.clone() }],
        };
        let script = format!("cargo run -q 2>&1; ls /build | head -1; echo | socat - TCP:127.0.0.1:{port}; cat ~/.cargo/credentials.toml 2>/dev/null && echo LEAKED");
        let output = sandbox.claude(Path::new("/usr/bin/bash")).args(["-c", &script]).output().unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        let _ = bridge.kill();

        assert!(said.contains("pid true"), "the crate built and ran offline:\n{said}\n{}", String::from_utf8_lossy(&output.stderr));
        assert!(said.contains("debug"), "and built into /build:\n{said}");
        assert!(said.contains("hello from the host"), "and the service answered:\n{said}");
        assert!(!said.contains("LEAKED"));
        assert!(root.join("build/debug").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_session_in_the_sandbox_gets_its_own_tools_and_no_agents_of_its_own() {
        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains::default(),
            time_limit: Duration::from_secs(60),
            scopes: false,
        };
        let sandbox = |writable| Sandbox {
            project: PathBuf::from("/srv/project"),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
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
            },
            time_limit: Duration::from_secs(60),
            scopes: true,
        };
        let arguments = arguments_of(&Sandbox {
            project: PathBuf::from("/home/someone/project"),
            profile: PathBuf::from("/data/reviews/r1/profile"),
            outside: &outside,
            broker_socket: Some(PathBuf::from("/run/anna/hand-h1.sock")),
            writable: false,
            build_dir: PathBuf::from("/data/builds/abc"),
            services: Vec::new(),
        });

        let bwrap = arguments.iter().position(|it| it == "bwrap").expect("with a scope to be had, bwrap runs inside one");
        assert!(arguments[..bwrap].contains(&"--scope".to_string()));
        assert!(arguments[..bwrap].iter().any(|it| it.starts_with("MemoryMax=")));
        assert!(arguments[..bwrap].iter().any(|it| it.starts_with("TasksMax=")));

        assert_eq!(binds(&arguments, "--bind"), [("/data/reviews/r1/profile", "/profile"), ("/data/builds/abc", BUILD_INSIDE)]);
        assert!(binds(&arguments, "--ro-bind").contains(&(installs.to_str().unwrap(), installs.to_str().unwrap())));
        assert!(arguments.windows(3).any(|it| {
            it[0] == "--setenv" && it[1] == "PATH" && it[2] == "/home/someone/.local/share/mise/installs/ruby/3.4.7/bin:/usr/bin"
        }));
        assert!(binds(&arguments, "--ro-bind").contains(&("/home/someone/project", "/work")));
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/hand-h1.sock", BROKER_INSIDE)));
    }
}
