//! Benchmarks encoding, hashing and signing objects.

#![expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use origin_shared::{
    AcceptanceBody, Base, ChangeId, Digest, EncodedObject, EntryKind, EntryName, Inputs, Kind,
    NodeKey, Object, ObjectId, Record, RecordId, RepoPath, Revision, Source, Statement, TargetName,
    Tree, TreeEntry, Verdict,
};

fn main() {
    divan::main();
}

/// Benchmarks hashing a blob of `len` bytes, as snapshotting a changed
/// file does.
#[divan::bench(args = [4 * 1024, 1024 * 1024, 64 * 1024 * 1024])]
fn hash_blob(bencher: Bencher<'_, '_>, len: usize) {
    let content = vec![0xa5_u8; len];
    bencher
        .counter(BytesCount::new(len))
        .bench(|| EncodedObject::blob(black_box(&content)));
}

#[expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]
fn tree(len: usize) -> Tree {
    (0..len)
        .map(|i| {
            let name = EntryName::new(format!("file-{i:06}.rs")).expect("the name is valid");
            let id = EncodedObject::blob(&i.to_le_bytes()).id();
            let entry = TreeEntry {
                kind: EntryKind::File,
                id,
            };
            (name, entry)
        })
        .collect()
}

/// Benchmarks encoding and hashing a tree of `len` entries.
#[divan::bench(args = [16, 1024])]
fn encode_tree(bencher: Bencher<'_, '_>, len: usize) {
    let tree = Object::Tree(tree(len));
    bencher.bench(|| black_box(&tree).encode());
}

/// Benchmarks decoding a tree of `len` entries, as every tree read does.
#[divan::bench(args = [16, 1024])]
#[expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]
fn decode_tree(bencher: Bencher<'_, '_>, len: usize) {
    let encoded = Object::Tree(tree(len)).encode();
    bencher.bench(|| {
        black_box(&encoded)
            .decode_as::<Tree>()
            .expect("the object decodes")
    });
}

/// Builds an id of any type by hashing `seed` as a blob.
fn id<T: From<ObjectId>>(seed: &str) -> T {
    EncodedObject::blob(seed.as_bytes()).id().into()
}

/// Builds a CI result with declared inputs, the most common statement.
fn statement() -> Statement {
    Statement {
        subject: RecordId::Revision(id("revision")),
        kind: Kind::new("ci.test").expect("the name is valid"),
        verdict: Some(Verdict::Pass),
        issuer: "node:ci-1".parse().expect("the text parses"),
        inputs: Some(Inputs {
            paths: ["src", "Cargo.toml", "Cargo.lock"]
                .map(|path| RepoPath::new(path).expect("the name is valid"))
                .into(),
            target_paths: Vec::new(),
            definition: Digest::of(b"cargo test"),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external: false,
        }),
        payload: Some(id("log")),
        supersedes: None,
        promoted_from: None,
    }
}

/// Benchmarks encoding and hashing a statement.
#[divan::bench]
fn encode_statement(bencher: Bencher<'_, '_>) {
    let statement = statement();
    bencher.bench(|| black_box(&statement).encode());
}

/// Benchmarks decoding a statement, as every statement read does.
#[divan::bench]
#[expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]
fn decode_statement(bencher: Bencher<'_, '_>) {
    let encoded = statement().encode();
    bencher.bench(|| {
        black_box(&encoded)
            .decode_as::<Statement>()
            .expect("the object decodes")
    });
}

/// Benchmarks encoding and hashing a revision.
#[divan::bench]
#[expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]
fn encode_revision(bencher: Bencher<'_, '_>) {
    let revision = Revision {
        change: ChangeId::from_bytes([7; 16]),
        predecessors: vec![id("old")],
        base: Base::Acceptance(id("main")),
        deps: Vec::new(),
        snapshot: id("tree"),
        description: id("description"),
        author: "user:alice".parse().expect("the text parses"),
    };
    bencher.bench(|| black_box(&revision).encode());
}

fn acceptance() -> AcceptanceBody {
    AcceptanceBody {
        target: TargetName::MAIN.id(),
        seq: 41,
        prev: Some(id("prev")),
        state: id("tree"),
        landed: vec![Source::Revision(id("revision"))],
        evidence: vec![statement().encode().id().into()],
        policy: Some(id("policy")),
    }
}

/// Benchmarks signing an acceptance. A target's authority signs one per
/// landing.
#[divan::bench]
fn sign_acceptance(bencher: Bencher<'_, '_>) {
    let key = NodeKey::from_bytes(&[42; 32]);
    let body = acceptance();
    bencher
        .with_inputs(|| body.clone())
        .bench_values(|body| body.sign(&key));
}

/// Benchmarks verifying an acceptance. A fetch verifies each acceptance
/// of every chain it brings.
#[divan::bench]
#[expect(clippy::expect_used, reason = "A benchmark fails by panicking.")]
fn verify_acceptance(bencher: Bencher<'_, '_>) {
    let key = NodeKey::from_bytes(&[42; 32]);
    let public = key.public();
    let acceptance = acceptance().sign(&key);
    bencher.bench(|| {
        black_box(&acceptance)
            .verify(&public)
            .expect("the signature verifies");
    });
}
