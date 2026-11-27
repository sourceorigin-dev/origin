//! End-to-end tests of `origin ci` as a tool in a working copy, through
//! the real `origin` and `origin-ci` binaries.

#[path = "../../origin-src/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::Repo;
use origin_platform::{Native, Processes};
use origin_shared::{Digest, Inputs, RecordId};
use origin_src::annotations::Annotations;
use origin_src::store::Store;

/// Returns a `checks.toml` whose `test` check reads `src` (and, like every
/// check, `checks.toml`), and appends a line to `log` each time it runs.
fn checks(log: &Path) -> String {
    format!(
        "[checks.test]\ncommand = \"test -f src/lib.rs && echo tested && echo ran >> {}\"\n\
         inputs = [\"src\"]\ntoolchain = \"echo toolchain 1\"\n",
        log.display()
    )
}

/// Returns how many times the check logging to `log` ran.
fn runs(log: &Path) -> usize {
    fs::read_to_string(log).map_or(0, |log| log.lines().count())
}

#[test]
fn tool_mode_runs_checks_and_promotes_unchanged_results() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write(
        "checks.toml",
        &(checks(&log)
            + "[checks.lint]\ncommand = \"echo lint failed; exit 1\"\ninputs = [\"src\"]\n"),
    );

    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "lint fails: {stdout}");
    assert!(stdout.contains("ci.lint fail in "), "{stdout}");
    assert!(
        stdout.contains("lint failed\n"),
        "a failure shows its output: {stdout}"
    );
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 1);
    let statements = repo.ok(&["statements"]);
    assert!(
        statements.contains("ci.test pass by user:ada\n"),
        "{statements}"
    );
    assert!(statements.contains("    payload: tested\n"), "{statements}");

    let again = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(again.stdout).unwrap();
    assert!(stdout.contains("ci.test pass, already ran"), "{stdout}");
    assert_eq!(runs(&log), 1);

    repo.write("docs/guide.md", "# Guide\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass, promoted"), "{stdout}");
    assert!(stdout.contains("ci.lint fail, promoted"), "{stdout}");
    assert_eq!(runs(&log), 1);

    repo.write("src/lib.rs", "fn b() {}\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 2);

    repo.ok(&["annotate", "@", "ci.test", "--withdraw"]);
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 3);

    let state = repo.config().join("state/origin-ci");
    let [key] = &fs::read_dir(&state).unwrap().collect::<Vec<_>>()[..] else {
        panic!("one repository's state");
    };
    let runs = key.as_ref().unwrap().path().join("runs");
    assert_eq!(fs::read_dir(runs).unwrap().count(), 0);
}

/// Returns the inputs of the `kind` statement about `revision`, from the
/// repository's records.
fn inputs_of(repo: &Repo, revision: &str, kind: &str) -> Inputs {
    let store = Store::new(repo.path(".origin"), Native::default());
    let revision = RecordId::Revision(revision.parse().unwrap());
    let about = Annotations::open(&store).unwrap().about(revision).unwrap();
    let current = about
        .into_iter()
        .rev()
        .find(|annotation| annotation.statement.kind.as_str() == kind)
        .unwrap();
    current.statement.inputs.unwrap()
}

#[test]
fn exec_runs_a_command_as_a_check() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.ok(&["describe", "First"]);
    let first = repo.revision();
    repo.ok(&["new"]);
    repo.write("src/lib.rs", "fn b() {}\n");
    let exec = |command: &[&str]| {
        let args = [
            &[
                "ci", "exec", "lint", "--rev", &first, "--inputs", "src", "--",
            ],
            command,
        ];
        let output = repo.run(repo.root(), &args.concat());
        let stdout = String::from_utf8(output.stdout).unwrap();
        (output.status.success(), stdout)
    };

    let reads = ["sh", "-c", "cat src/lib.rs; printf '%s\\n' \"$0\"", "a b"];
    let (passed, stdout) = exec(&reads);
    assert!(passed && stdout.starts_with("ci.lint pass in "), "{stdout}");
    let statements = repo.ok(&["statements", &first]);
    assert!(
        statements.contains("ci.lint pass by user:ada\n"),
        "{statements}"
    );
    assert!(statements.contains("    inputs: src\n"), "{statements}");
    assert!(
        statements.contains("    payload: fn a() {}\n"),
        "{statements}"
    );
    let definition = r#"sh -c 'cat src/lib.rs; printf '\''%s\n'\'' "$0"' 'a b'"#;
    let inputs = inputs_of(&repo, &first, "ci.lint");
    assert_eq!(inputs.definition, Digest::of(definition.as_bytes()));

    assert!(exec(&reads).1.starts_with("ci.lint pass in "));
    let (passed, stdout) = exec(&["sh", "-c", "echo broken; exit 3"]);
    assert!(!passed, "{stdout}");
    assert!(stdout.starts_with("ci.lint fail in "), "{stdout}");
    assert!(stdout.ends_with("broken\n"), "its output: {stdout}");
    let statements = repo.ok(&["statements", &first]);
    assert_eq!(
        statements.matches(" (superseded by ").count(),
        2,
        "{statements}"
    );
    assert!(
        statements.contains("ci.lint fail by user:ada\n"),
        "{statements}"
    );

    repo.write("src/lib.rs", "fn a() {}\n");
    repo.ok(&["describe", "Same library"]);
    let promoted = repo.ok(&["promote"]);
    assert!(
        promoted.contains("Promoted ci.lint fail by user:ada"),
        "{promoted}"
    );
}

#[test]
fn a_failed_exec_wins_over_a_promoted_pass() {
    let repo = Repo::new();
    repo.require_tests();
    let exec = |command: &str| {
        let args = ["ci", "exec", "test", "--inputs", "src", "--", command];
        repo.run(repo.root(), &args).status.success()
    };
    repo.ok(&["new", "--on", "main", "Library"]);
    repo.write("src/lib.rs", "fn a() {}\n");
    assert!(exec("true"));
    assert!(repo.ok(&["land"]).contains(" on main #1\n"));

    repo.ok(&["new", "--on", "main", "Docs"]);
    repo.write("docs.md", "docs\n");
    assert!(repo.ok(&["promote"]).contains("Promoted ci.test pass"));
    assert!(!exec("false"));
    let landing = repo.ok(&["land"]);
    assert!(landing.starts_with("The landing of change "), "{landing}");
}

#[test]
fn runs_replace_promotions_a_result_there_replaces() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("checks.toml", &checks(&log));
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    let promoted = |file: &str| {
        repo.ok(&["new"]);
        repo.write(file, "docs\n");
        assert!(run().contains("ci.test pass, promoted"));
    };
    let ran = |runs_now| {
        let stdout = run();
        assert!(stdout.contains("ci.test pass in "), "{stdout}");
        assert_eq!(runs(&log), runs_now);
    };

    promoted("failed.md");
    repo.ok(&["annotate", "@", "ci.test", "--fail"]);
    ran(2);
    promoted("withdrawn.md");
    repo.ok(&["annotate", "@", "ci.test", "--neutral"]);
    repo.ok(&["annotate", "@", "ci.test", "--withdraw"]);
    ran(3);
}

#[test]
fn a_promotion_of_a_result_taken_back_covers_nothing() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    repo.write("src/lib.rs", "fn a() {}\n");
    repo.write("checks.toml", &checks(&log));
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    let first = repo.revision();
    repo.ok(&["new"]);
    repo.write("docs.md", "docs\n");
    assert!(run().contains("ci.test pass, promoted"));

    repo.ok(&["annotate", &first, "ci.test", "--withdraw"]);
    let stdout = run();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 2);
}

#[test]
fn checks_run_in_checkouts_of_their_own() {
    let repo = Repo::new();
    repo.write("input", "broken\n");
    repo.write(
        "checks.toml",
        "[checks.fix]\ncommand = \"echo fixed > input\"\n\
         [checks.verify]\ncommand = \"grep -q fixed input\"\n",
    );
    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "{stdout}");
    assert!(stdout.contains("ci.fix pass in "), "{stdout}");
    assert!(
        stdout.contains("ci.verify fail in "),
        "it never sees what ci.fix wrote: {stdout}"
    );
    assert_eq!(repo.read("input"), "broken\n");
}

#[test]
fn toolchains_are_told_apart_by_all_their_output() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    let version = |version: &str| {
        fs::write(&tools, format!("tool {version}\n{}", "x".repeat(100_000))).unwrap();
    };
    version("1");
    repo.write(
        "checks.toml",
        &format!(
            "[checks.test]\ncommand = \"true\"\ntoolchain = \"cat {}\"\n",
            tools.display()
        ),
    );
    let run = || String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(run().contains("ci.test pass in "));
    version("2");
    let again = run();
    assert!(again.contains("ci.test pass in "), "{again}");
}

#[test]
fn checks_never_see_the_runners_token() {
    let repo = Repo::new();
    repo.write(
        "checks.toml",
        "[checks.token]\ncommand = 'test -z \"$ORIGIN_TOKEN\"'\n",
    );
    let output = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ci.token pass in "), "{stdout}");
}

/// Whether the process `pid` is running, as `kill -0` finds it.
fn alive(pid: &str) -> bool {
    let probe = std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status();
    probe.unwrap().success()
}

/// Waits for `path` to be written, once, and returns its content. Fails
/// after a minute.
#[expect(clippy::panic, reason = "A test helper fails by panicking.")]
fn written(path: &Path) -> String {
    for _ in 0..3000 {
        match fs::read_to_string(path) {
            Ok(text) if text.ends_with('\n') => return text.trim_end().to_owned(),
            _ => thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("nothing wrote {}", path.display());
}

/// Waits until `pid` is gone. Fails after a few seconds.
#[expect(clippy::panic, reason = "A test helper fails by panicking.")]
fn gone(pid: &str) {
    for _ in 0..250 {
        if !alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} still runs");
}

/// Waits until `pid`, which a check left running, is gone. `origin-ci`
/// kills it where the platform tells which group holds which files (see
/// `Processes::SEES_HELD_FILES`); elsewhere the test kills it, so nothing
/// is left running.
fn ended(pid: &str) {
    if !Native::SEES_HELD_FILES {
        let _ = std::process::Command::new("kill")
            .args(["-9", pid])
            .status();
    }
    gone(pid);
}

#[test]
fn checks_leave_nothing_running_once_they_end_or_their_run_is_killed() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let check = |command: &str| {
        let command = command.replace("{started}", &started.display().to_string());
        repo.write(
            "checks.toml",
            &format!("[checks.test]\ncommand = \"{command}\"\n"),
        );
        let _ = fs::remove_file(&started);
    };

    check("sleep 60 & echo $! > {started}");
    let ran = Instant::now();
    let output = repo.run(repo.root(), &["ci", "run"]);
    assert_eq!(output.status.success(), Native::SEES_HELD_FILES);
    assert!(
        ran.elapsed() < Duration::from_secs(30),
        "it waited for sleep"
    );
    ended(&written(&started));

    check("sleep 60 & echo $! > {started}; wait");
    let (mut run, _) = repo.spawn(&["ci", "run"]);
    let sleep = written(&started);
    run.kill().unwrap();
    run.wait().unwrap();
    assert!(alive(&sleep), "nothing kills it yet");
    check("true");
    assert!(repo.run(repo.root(), &["ci", "run"]).status.success());
    ended(&sleep);
}
