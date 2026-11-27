use std::slice;

use super::*;

const LINT_AND_LINKS: &str = "[checks.lint]\ncommand = \"cargo clippy\"\n\
        inputs = [\"src\", \"Cargo.toml\"]\ntoolchain = \"rustc -V\"\n\
        [checks.links]\ncommand = \"lychee docs\"\nexternal = true\n";

#[test]
fn parses_checks_by_kind() {
    let checks = Checks::parse(LINT_AND_LINKS, Defined::InSnapshot).unwrap();
    let lint = checks.get(&Kind::new("ci.lint").unwrap()).unwrap();
    assert_eq!(
        lint.inputs,
        [
            RepoPath::new("src").unwrap(),
            RepoPath::new("Cargo.toml").unwrap(),
            RepoPath::new("checks.toml").unwrap(),
        ],
        "the checks are an input of each"
    );
    assert_eq!(lint.target_inputs, []);
    assert_eq!(lint.toolchain.as_deref(), Some("rustc -V"));
    let links = checks.get(&Kind::new("ci.links").unwrap()).unwrap();
    assert_eq!(Checks::kind("links").unwrap(), links.kind);
    assert_eq!(links.inputs, [RepoPath::root()], "the whole snapshot");
    assert!(links.external && links.inputs(b"").external);
    assert_eq!(checks.iter().count(), 2);

    let parse = |text| Checks::parse(text, Defined::InSnapshot);
    assert!(
        parse("[checks.Lint]\ncommand = \"x\"\n").is_err(),
        "not a kind"
    );
    assert!(
        parse("[checks.lint]\ncmd = \"x\"\n").is_err(),
        "unknown key"
    );
    assert_eq!(parse("").unwrap(), Checks::default());
}

#[test]
fn checks_in_force_on_a_target_read_it_there() {
    let checks = Checks::parse(LINT_AND_LINKS, Defined::OnTarget).unwrap();
    let file = RepoPath::new(Checks::FILE).unwrap();
    for check in checks.iter() {
        assert_eq!(check.target_inputs, slice::from_ref(&file), "{check:?}");
        assert!(!check.inputs.contains(&file), "{check:?}");
    }
    let lint = checks.get(&Kind::new("ci.lint").unwrap()).unwrap();
    let in_snapshot = Checks::parse(LINT_AND_LINKS, Defined::InSnapshot).unwrap();
    let ran = lint.inputs(b"rustc 1.99.0");
    assert_eq!(ran.target_paths, [file]);
    assert_ne!(
        ran,
        in_snapshot.get(&lint.kind).unwrap().inputs(b"rustc 1.99.0")
    );
}

#[test]
fn inputs_follow_the_command_and_toolchain() {
    let check = |command: &str| Check {
        kind: Kind::new("ci.test").unwrap(),
        command: command.to_owned(),
        inputs: vec![RepoPath::new("src").unwrap()],
        target_inputs: Vec::new(),
        toolchain: None,
        external: false,
    };
    let inputs = check("cargo test").inputs(b"rustc 1.99.0");
    assert_eq!(inputs, check("cargo test").inputs(b"rustc 1.99.0"));
    assert_ne!(
        inputs,
        check("cargo test --release").inputs(b"rustc 1.99.0")
    );
    assert_ne!(inputs, check("cargo test").inputs(b"rustc 1.100.0"));
}
