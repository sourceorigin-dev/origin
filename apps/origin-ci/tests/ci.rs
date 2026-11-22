//! End-to-end tests of `origin ci`, through the real `origin` and
//! `origin-ci` binaries, as a tool in a working copy.

#[path = "../../origin-src/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::Repo;
use origin_platform::{Native, Processes};

/// A `checks.toml` whose `test` reads `src`, and, as every check,
/// `checks.toml`, and appends a line to `log` each time it runs.
fn checks(log: &Path) -> String {
    format!(
        "[checks.test]\ncommand = \"test -f src/lib.rs && echo tested && echo ran >> {}\"\n\
         inputs = [\"src\"]\ntoolchain = \"echo toolchain 1\"\n",
        log.display()
    )
}

/// How many times the check logging to `log` ran.
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

    // Running again finds the results.
    let again = repo.run(repo.root(), &["ci", "run"]);
    let stdout = String::from_utf8(again.stdout).unwrap();
    assert!(stdout.contains("ci.test pass, already ran"), "{stdout}");
    assert_eq!(runs(&log), 1);

    // A docs-only change keeps both results, promoted.
    repo.write("docs/guide.md", "# Guide\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass, promoted"), "{stdout}");
    assert!(stdout.contains("ci.lint fail, promoted"), "{stdout}");
    assert_eq!(runs(&log), 1);

    // A change to `src` runs both again.
    repo.write("src/lib.rs", "fn b() {}\n");
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 2);

    // A withdrawn result is none: it runs again.
    repo.ok(&["annotate", "@", "ci.test", "--withdraw"]);
    let stdout = String::from_utf8(repo.run(repo.root(), &["ci", "run"]).stdout).unwrap();
    assert!(stdout.contains("ci.test pass in "), "{stdout}");
    assert_eq!(runs(&log), 3);

    // Nothing is left of the checkouts.
    let state = repo.config().join("state/origin-ci");
    let [key] = &fs::read_dir(&state).unwrap().collect::<Vec<_>>()[..] else {
        panic!("one repository's state");
    };
    let runs = key.as_ref().unwrap().path().join("runs");
    assert_eq!(fs::read_dir(runs).unwrap().count(), 0);
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
    // Each docs-only change gets the pass promoted, then a statement of
    // its own: `ci run` doesn't take the promotion for a result then.
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

    // Its issuer takes the original back: the promotion holds nothing.
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
    // Longer than the output a check keeps.
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

/// Whether the process `pid` runs, as `kill -0` finds it.
fn alive(pid: &str) -> bool {
    let probe = std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status();
    probe.unwrap().success()
}

/// Waits for what `path` holds, written once, failing after a minute.
fn written(path: &Path) -> String {
    for _ in 0..3000 {
        match fs::read_to_string(path) {
            Ok(text) if text.ends_with('\n') => return text.trim_end().to_owned(),
            _ => thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("nothing wrote {}", path.display());
}

/// Waits until `pid` is gone, failing after a few seconds.
fn gone(pid: &str) {
    for _ in 0..250 {
        if !alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} still runs");
}

/// Waits until `pid`, which a check left running, is gone: killed by
/// `origin-ci` where the platform tells which group holds what (see
/// `Processes::SEES_HELD_FILES`), or else by the test, so it leaves nothing
/// running.
fn ended(pid: &str) {
    if !Native::SEES_HELD_FILES {
        // Whether it ran still or not, `gone` tells.
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

    // What a check leaves running is killed once it ends, without waiting
    // for it to close the output it holds; where the platform can't tell
    // which group still holds what it started, it escapes instead, and the
    // run waits for it to close that output.
    check("sleep 60 & echo $! > {started}");
    let ran = Instant::now();
    let output = repo.run(repo.root(), &["ci", "run"]);
    assert!(output.status.success());
    if Native::SEES_HELD_FILES {
        assert!(
            ran.elapsed() < Duration::from_secs(30),
            "it waited for sleep"
        );
    }
    ended(&written(&started));

    // A run killed while its check runs leaves it running, until the next
    // run sweeps its checkout.
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
