//! Helpers for the end-to-end tests, which run the real `origin` binary.
//! Plugin tests in other packages include them too.

#![allow(
    dead_code,
    reason = "each test binary uses its own subset of the helpers"
)]

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Returns the path of the `origin` binary: this package's, or for a
/// plugin's tests, the one built next to the plugin in the same target
/// directory.
pub fn origin() -> PathBuf {
    if let Some(origin) = option_env!("CARGO_BIN_EXE_origin") {
        return PathBuf::from(origin);
    }
    let exe = env::current_exe().unwrap();
    let origin = exe
        .ancestors()
        .map(|dir| dir.join("origin"))
        .find(|origin| origin.is_file())
        .unwrap_or_default();
    assert!(
        origin.exists(),
        "build `origin` first, such as with `cargo build --bin origin`"
    );
    origin
}

/// Returns `$PATH` with the directory holding `origin` and the plugins built
/// with it added first.
pub fn path_with_plugins() -> OsString {
    let bin = origin().parent().unwrap().to_owned();
    let paths = env::var_os("PATH").unwrap_or_default();
    env::join_paths([bin].into_iter().chain(env::split_paths(&paths))).unwrap()
}

/// A user with their own configuration directory, and so their own node
/// key, and the token they send to servers, if any. Clones share the
/// directory.
#[derive(Clone)]
pub struct User {
    config: Arc<tempfile::TempDir>,
    token: Option<String>,
}

impl User {
    /// Creates a user named `name`, whose node is named after them, with the
    /// token [`Server`] knows them by.
    pub fn new(name: &str) -> Self {
        Self {
            token: Some(Server::token(name)),
            ..Self::anonymous(name)
        }
    }

    /// Creates a user named `name` without a token, whom servers don't know.
    pub fn anonymous(name: &str) -> Self {
        let config = tempfile::tempdir().unwrap();
        fs::write(
            config.path().join("config.toml"),
            format!("user = \"{name}\"\n"),
        )
        .unwrap();
        Self {
            config: Arc::new(config),
            token: None,
        }
    }

    /// Returns a copy of the user that sends `token` instead.
    pub fn with_token(&self, token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            ..self.clone()
        }
    }

    /// Returns the user's configuration directory.
    pub fn config(&self) -> &Path {
        self.config.path()
    }

    /// Builds an `origin` command with `args` that runs in `cwd` as this user,
    /// with the plugins built with it on `$PATH`. Plugins keep their state in
    /// the user's configuration directory.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(origin());
        command
            .args(args)
            .current_dir(cwd)
            .env("ORIGIN_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.config().join("state"))
            .env("PATH", path_with_plugins())
            .env_remove("ORIGIN_TOKEN");
        if let Some(token) = &self.token {
            command.env("ORIGIN_TOKEN", token);
        }
        command
    }
}

/// A temporary working copy and its user.
pub struct Repo {
    dir: tempfile::TempDir,
    user: User,
}

impl Repo {
    /// Creates a repository owned by a new user, `ada`.
    pub fn new() -> Self {
        Self::init(&User::new("ada"))
    }

    /// Creates a repository owned by `user`.
    pub fn init(user: &User) -> Self {
        let repo = Self::empty(user);
        repo.ok(&["init"]);
        repo
    }

    /// Clones `url` as `user`, passing `args` to `clone` as well.
    pub fn clone(user: &User, url: &str, args: &[&str]) -> Self {
        let repo = Self::empty(user);
        let root = repo.root().to_str().unwrap().to_owned();
        repo.ok(&[&["clone", url, &root], args].concat());
        repo
    }

    fn empty(user: &User) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            user: user.clone(),
        }
    }

    /// The root of the working copy.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Returns the user's configuration directory.
    pub fn config(&self) -> &Path {
        self.user.config()
    }

    /// Returns the repository's user.
    pub fn user(&self) -> &User {
        &self.user
    }

    /// Runs `origin` in `cwd` as the repository's user.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    /// Builds an `origin` command with `args` that runs in `cwd` as the
    /// repository's user.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        self.user.command(cwd, args)
    }

    /// Starts a long-running command, such as `watch`. Returns the process and a
    /// receiver of its output lines as they arrive.
    pub fn spawn(&self, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
        let mut child = self
            .user
            .command(self.root(), args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        (child, receive)
    }

    /// Returns the full id of the working copy's revision.
    pub fn revision(&self) -> String {
        self.read(".origin/workspace").trim_end().to_owned()
    }

    /// Runs a command that must succeed, returning its output.
    pub fn ok(&self, args: &[&str]) -> String {
        self.ok_in(self.root(), args)
    }

    /// Runs a command in `cwd` that must succeed, returning its output.
    pub fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "`origin {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that must fail, returning its error.
    pub fn fails(&self, args: &[&str]) -> String {
        self.fails_in(self.root(), args)
    }

    /// Runs `origin` in `cwd`, which must fail, and returns its errors.
    pub fn fails_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            !output.status.success(),
            "`origin {}` succeeded",
            args.join(" ")
        );
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    pub fn write(&self, path: &str, content: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.path(path)).unwrap()
    }

    /// The short id of the working copy's change.
    pub fn change(&self) -> String {
        let status = self.ok(&["status"]);
        let line = status.lines().next().unwrap();
        let headline = line.strip_prefix("Working copy: ").unwrap();
        headline.split(' ').next().unwrap().to_owned()
    }

    /// Returns the full id of the working copy's change.
    pub fn full_change(&self) -> String {
        let show = self.ok(&["show"]);
        let line = show.lines().next().unwrap();
        let change = line.strip_prefix("Change ").unwrap();
        change.split(' ').next().unwrap().to_owned()
    }

    /// Lands a policy that makes `main` require a passing `ci.test` result from
    /// the `runners` group, which includes `ada`.
    pub fn require_tests(&self) {
        self.ok(&["new", "--on", "policy", "Require tests"]);
        let policy = self
            .read("policy.toml")
            .replace("# [groups]", "[groups]")
            .replace(
                "# runners = [\"node:ci-1\"]",
                "runners = [\"node:ci-1\", \"user:ada\"]",
            )
            .replace("# require", "require");
        self.write("policy.toml", &policy);
        assert!(self.ok(&["land"]).contains(" on policy #1\n"));
    }

    /// Creates a workspace at `path` and returns its change.
    pub fn workspace(&self, path: &Path, args: &[&str]) -> String {
        let path = path.to_str().unwrap();
        let output = self.ok(&[&["workspace", "new", path], args].concat());
        let line = output.lines().nth(1).unwrap();
        let headline = line.strip_prefix("Working copy: ").unwrap();
        headline.split(' ').next().unwrap().to_owned()
    }

    /// Returns the path of the file in which the store keeps the object holding
    /// `content`.
    pub fn stored_object(&self, content: &[u8]) -> PathBuf {
        let hex = origin_shared::EncodedObject::blob(content).id().to_hex();
        self.path(&format!(
            ".origin/objects/loose/{}/{}",
            &hex[..2],
            &hex[2..]
        ))
    }

    /// Runs `origin <args>` in `cwd` until it reads the stored object holding
    /// `content`. Once `ready` returns true, kills the process, as a crash
    /// would, and then stores the object again.
    pub fn crash_reading(
        &self,
        cwd: &Path,
        content: &[u8],
        args: &[&str],
        ready: impl Fn() -> bool,
    ) {
        let stored = self.stored_object(content);
        let object = fs::read(&stored).unwrap();
        fs::remove_file(&stored).unwrap();
        let fifo = Command::new("mkfifo").arg(&stored).status().unwrap();
        assert!(fifo.success());
        let mut child = self
            .user
            .command(cwd, args)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // wall-clock: opening the pipe blocks until the command opens it, so it is polled.
        let (opened, open) = mpsc::channel();
        let pipe = stored.clone();
        thread::spawn(move || opened.send(OpenOptions::new().write(true).open(pipe)));
        let deadline = Instant::now() + Duration::from_secs(20);
        let _pipe = loop {
            if let Ok(pipe) = open.recv_timeout(Duration::from_millis(20)) {
                break pipe.unwrap();
            }
            assert!(child.try_wait().unwrap().is_none(), "it never read it");
            assert!(Instant::now() < deadline, "it never read it in time");
        };
        while !ready() {
            assert!(Instant::now() < deadline, "never ready");
            thread::sleep(Duration::from_millis(20));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_file(&stored).unwrap();
        fs::write(&stored, object).unwrap();
        fs::set_permissions(&stored, fs::Permissions::from_mode(0o444)).unwrap();
    }
}

/// A running `origin serve` hosting one repository, `demo`, which the
/// server's node, `hosted`, orders. Each of [`Server::USERS`] signs in with
/// [`Server::token`]. The server is killed when dropped.
pub struct Server {
    dir: tempfile::TempDir,
    child: Child,
    /// The server's base URL.
    pub base: String,
}

impl Server {
    /// The users the server knows.
    pub const USERS: [&str; 4] = ["ada", "bob", "carol", "alice"];

    /// Returns the static token of `user`.
    pub fn token(user: &str) -> String {
        format!("token-{user}")
    }

    #[expect(clippy::panic, reason = "A test helper fails by panicking.")]
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("origin-server.toml");
        let tokens: Vec<_> = Self::USERS
            .iter()
            .map(|user| format!("\"user:{user}\" = \"{}\"", Self::token(user)))
            .collect();
        let tokens = tokens.join("\n");
        fs::write(
            &config,
            format!(
                "listen = \"127.0.0.1:0\"\nnode = \"hosted\"\n[repos]\ndemo = \"repos/demo\"\n\
                 [tokens]\n{tokens}"
            ),
        )
        .unwrap();
        let origin = |args: &[&str]| {
            let mut command = Command::new(origin());
            command.args(args).arg("--config").arg(&config);
            command
        };
        let created = origin(&["repo", "create"])
            .arg(dir.path().join("repos/demo"))
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let mut child = origin(&["serve"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        let read = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
        let Some(base) = line.trim().strip_prefix("Listening on ") else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`origin serve` printed {line:?} ({read:?}), not its address");
        };
        let base = base.to_owned();
        Self { dir, child, base }
    }

    /// Returns the directory that holds the server's configuration, key and
    /// repositories.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Returns the URL of `demo`.
    pub fn url(&self) -> String {
        format!("{}/demo", self.base)
    }

    /// Adds `rules` to `demo`'s `policy.toml` and lands them on `policy`. It
    /// lands through the repository's path, as the server's own user, who runs
    /// its node.
    pub fn add_policy(&self, rules: &str) {
        let operator = User::anonymous("operator");
        fs::write(
            operator.config().join("config.toml"),
            "user = \"operator\"\nnode = \"hosted\"\n",
        )
        .unwrap();
        fs::copy(
            self.dir().join("node.key"),
            operator.config().join("node.key"),
        )
        .unwrap();
        let path = self.dir().join("repos/demo");
        let policy = Repo::clone(&operator, path.to_str().unwrap(), &["--target", "policy"]);
        let mut toml = policy.read("policy.toml");
        toml.push_str(rules);
        policy.write("policy.toml", &toml);
        policy.ok(&["describe", "Change the rules"]);
        let landed = policy.ok(&["land"]);
        assert!(landed.contains("Landed change"), "{landed}");
    }

    /// Signs a token for `principal` with `scope`, using the server's key.
    pub fn issue_token(&self, principal: &str, scope: &[&str]) -> String {
        let mut command = Command::new(origin());
        command
            .args(["token", "issue", "--as", principal, "--config"])
            .arg(self.dir().join("origin-server.toml"));
        for capability in scope {
            command.args(["--scope", capability]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
