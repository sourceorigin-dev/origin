//! End-to-end tests of sharing work: cloning, pushing, fetching and
//! pulling, over HTTP and through a local path.

mod common;

use std::fs;

use common::{Repo, Server, User};

#[test]
fn pushing_a_change_that_moved_on_the_server_is_refused() {
    let server = Server::start();
    let (ada, bob) = (User::new("ada"), User::new("bob"));
    let alice = Repo::clone(&ada, &server.url(), &[]);
    alice.write("file", "alice\n");
    alice.ok(&["describe", "Shared work"]);
    alice.ok(&["push"]);

    let bobs = Repo::clone(&bob, &server.url(), &[]);
    let change = alice.full_change();
    bobs.ok(&["fetch", "--change", &change]);
    bobs.ok(&["edit", &change]);
    assert_eq!(bobs.read("file"), "alice\n");
    bobs.write("file", "bob\n");
    bobs.ok(&["push", &change]);

    alice.write("file", "alice again\n");
    let error = alice.fails(&["push"]);
    assert!(error.contains("is stale"), "{error}");
    alice.ok(&["fetch"]);
    assert!(alice.ok(&["status"]).contains("Divergent"));
    alice.ok(&["converge", &change]);
    assert!(alice.ok(&["push"]).starts_with("Pushed "));
}

#[test]
fn clones_make_the_directories_they_go_in() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    let url = origin.root().to_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cloned = ada
        .command(dir.path(), &["clone", url, "x/y/name"])
        .output()
        .unwrap();
    assert!(cloned.status.success(), "{cloned:?}");
    let root = fs::canonicalize(dir.path().join("x/y/name")).unwrap();
    let undo = ada.command(&root, &["undo"]).output().unwrap();
    let error = String::from_utf8_lossy(&undo.stderr);
    assert!(
        error.contains("nothing to undo but the creation of"),
        "{error}"
    );

    let args = ["clone", url, "z/y/name", "--target", "nope"];
    assert!(!ada.command(dir.path(), &args).status().unwrap().success());
    assert!(!dir.path().join("z").exists());

    let link = dir.path().join("link");
    std::os::unix::fs::symlink("nowhere", &link).unwrap();
    assert!(
        !ada.command(dir.path(), &["clone", url, "link"])
            .status()
            .unwrap()
            .success()
    );
    assert!(link.symlink_metadata().is_ok(), "the symlink stays");
}

#[test]
fn clones_follow_one_target_and_fetch_others_on_demand() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("README.md", "main\n");
    origin.ok(&["land"]);
    origin.ok(&["target", "new", "release", "--from", "main"]);
    origin.ok(&["new", "--on", "release"]);
    origin.write("RELEASE.md", "v1\n");
    origin.ok(&["land"]);

    let url = origin.root().to_str().unwrap();
    let clone = Repo::clone(&User::new("bob"), url, &["--target", "release"]);
    assert_eq!(clone.read("RELEASE.md"), "v1\n");
    assert!(clone.ok(&["log"]).contains("◆ release #1"));
    let error = clone.fails(&["log", "main"]);
    assert!(error.contains("main"), "not followed: {error}");
    let fetched = clone.ok(&["fetch", "--target", "main"]);
    assert!(fetched.contains("`main`: now followed, at #1"), "{fetched}");
    assert!(clone.ok(&["log", "main"]).starts_with("◆ main #1"));

    let missing = clone.fails(&["clone", url, "elsewhere", "--target", "nope"]);
    assert!(missing.contains("no target `nope`"), "{missing}");
    assert!(
        !clone.path("elsewhere").exists(),
        "a failed clone leaves nothing"
    );
}

#[test]
fn a_move_that_cannot_fetch_moves_nothing() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("x", "x\n");
    origin.ok(&["describe", "X"]);
    let change = origin.full_change();
    let url = origin.root().to_str().unwrap();
    let clone = Repo::clone(&ada, url, &[]);
    clone.ok(&["fetch", "--change", &change]);

    let away = origin.root().with_extension("away");
    fs::rename(origin.root(), &away).unwrap();
    let error = clone.fails(&["edit", &change]);
    assert!(error.contains("remote"), "{error}");
    assert!(!clone.path(".origin/moving").exists());
    clone.write("notes", "mine\n");
    let status = clone.ok(&["status"]);
    assert!(status.contains("A notes\n"), "{status}");
    fs::rename(&away, origin.root()).unwrap();
    clone.ok(&["edit", &change]);
    assert_eq!(clone.read("x"), "x\n");
}

#[test]
fn a_clone_converges_heads_whose_merge_base_it_never_read() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("file", "base\n");
    origin.ok(&["describe", "Shared work"]);
    let change = origin.full_change();
    let pointer = origin.path(".origin/workspace");
    let started = fs::read_to_string(&pointer).unwrap();
    origin.write("file", "left\n");
    origin.ok(&["describe", "Shared work"]);
    fs::write(&pointer, started).unwrap();
    origin.write("file", "right\n");
    origin.ok(&["describe", "Shared work"]);

    let clone = Repo::clone(&ada, origin.root().to_str().unwrap(), &[]);
    clone.ok(&["fetch", "--change", &change]);
    let converged = clone.ok(&["converge", &change]);
    assert!(converged.starts_with("Converged change "), "{converged}");
    clone.ok(&["edit", &change]);
    let file = clone.read("file");
    assert!(
        file.contains("left\n") && file.contains("right\n"),
        "{file}"
    );
}

#[test]
fn a_repository_with_a_remote_is_no_remote() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    let clone = Repo::clone(&ada, origin.root().to_str().unwrap(), &[]);
    let url = clone.root().to_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cloned = ada
        .command(dir.path(), &["clone", url, "again"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&cloned.stderr);
    assert!(error.contains("has a remote of its own"), "{error}");
    assert!(!dir.path().join("again").exists());
    let other = Repo::init(&ada);
    other.ok(&["remote", url]);
    let error = other.fails(&["push"]);
    assert!(error.contains("has a remote of its own"), "{error}");
}
