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

use crate::toolchains::Toolchains;

const INSIDE: &str = r#"
socat TCP-LISTEN:3128,fork,bind=127.0.0.1 UNIX-CONNECT:/run/proxy.sock &
sleep 0.3
exec /opt/claude "$@"
"#;

pub const BROKER_INSIDE: &str = "/run/broker.sock";
const GIT_PARTS_THAT_RUN_CODE: [&str; 3] = [".git/config", ".git/hooks", ".git/modules"];

/// What every sandbox of this process is given from outside: the way to
/// Claude, and the tools it can run.
pub struct Outside {
    pub proxy_socket: PathBuf,
    pub toolchains: Toolchains,
    pub time_limit: Duration,
}

pub struct Sandbox<'outside> {
    pub project: PathBuf,
    pub profile: PathBuf,
    pub outside: &'outside Outside,
    pub broker_socket: Option<PathBuf>,
    pub writable: bool,
}

impl Sandbox<'_> {
    /// `claude` inside the sandbox; whatever arguments the caller adds go to
    /// claude.
    pub fn claude(&self, claude_binary: &Path) -> Command {
        let mut command = Command::new("bwrap");
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
        if let Some(installs) = &self.outside.toolchains.installs {
            command.arg("--ro-bind").arg(installs).arg(installs);
        }
        command
            .arg("--ro-bind")
            .arg(claude_binary)
            .arg("/opt/claude")
            .arg("--bind")
            .arg(&self.profile)
            .arg("/profile")
            .arg("--ro-bind")
            .arg(&self.outside.proxy_socket)
            .arg("/run/proxy.sock");
        if let Some(broker_socket) = &self.broker_socket {
            command.arg("--ro-bind").arg(broker_socket).arg(BROKER_INSIDE);
        }

        command
            .args(["--chdir", "/work"])
            .args(["--setenv", "HOME", "/home/hand"])
            .args(["--setenv", "LANG", "C.UTF-8"])
            .args(["--setenv", "PATH", &self.outside.toolchains.path()])
            .args(["--setenv", "CLAUDE_CONFIG_DIR", "/profile"])
            .args(["--setenv", "HTTPS_PROXY", "http://127.0.0.1:3128"])
            .args(["/usr/bin/bash", "-c", INSIDE, "sandbox"]);
        command
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
        };
        let arguments = arguments_of(&Sandbox {
            project: project.clone(),
            profile: PathBuf::from("/data/hands/h1/profile"),
            outside: &outside,
            broker_socket: None,
            writable: true,
        });

        assert!(arguments.contains(&"--unshare-all".to_string()));
        let git = format!("{project_path}/.git");
        assert_eq!(
            binds(&arguments, "--bind"),
            [(project_path, "/work"), (git.as_str(), "/work/.git"), ("/data/hands/h1/profile", "/profile")]
        );
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

        let outside = Outside { proxy_socket: root.join("proxy.sock"), toolchains: Toolchains::default(), time_limit: Duration::from_secs(60) };
        let run = |project: &Path, script: &str| {
            let sandbox = Sandbox { project: project.to_path_buf(), profile: root.join("profile"), outside: &outside, broker_socket: None, writable: true };
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

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_reviewer_can_write_nothing_in_the_project() {
        let installs = PathBuf::from("/home/someone/.local/share/mise/installs");
        let outside = Outside {
            proxy_socket: PathBuf::from("/run/anna/proxy.sock"),
            toolchains: Toolchains {
                installs: Some(installs.clone()),
                bins: vec![installs.join("ruby/3.4.7/bin")],
            },
            time_limit: Duration::from_secs(60),
        };
        let arguments = arguments_of(&Sandbox {
            project: PathBuf::from("/home/someone/project"),
            profile: PathBuf::from("/data/reviews/r1/profile"),
            outside: &outside,
            broker_socket: Some(PathBuf::from("/run/anna/hand-h1.sock")),
            writable: false,
        });

        assert_eq!(binds(&arguments, "--bind"), [("/data/reviews/r1/profile", "/profile")]);
        assert!(binds(&arguments, "--ro-bind").contains(&(installs.to_str().unwrap(), installs.to_str().unwrap())));
        assert!(arguments.windows(3).any(|it| {
            it[0] == "--setenv" && it[1] == "PATH" && it[2] == "/home/someone/.local/share/mise/installs/ruby/3.4.7/bin:/usr/bin"
        }));
        assert!(binds(&arguments, "--ro-bind").contains(&("/home/someone/project", "/work")));
        assert!(binds(&arguments, "--ro-bind").contains(&("/run/anna/hand-h1.sock", BROKER_INSIDE)));
    }
}
