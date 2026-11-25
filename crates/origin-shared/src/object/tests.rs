use super::*;
use crate::{ChangeId, Digest, RepoPath, TargetName, TreeId};

/// A tree with an entry of each kind, also used by the tree tests.
pub(crate) fn sample_tree() -> Tree {
    [
        ("b.txt", EntryKind::File, EncodedObject::blob(b"b").id()),
        ("a", EntryKind::Tree, Tree::default().encode().id()),
        (
            "run.sh",
            EntryKind::Executable,
            EncodedObject::blob(b"#!").id(),
        ),
    ]
    .into_iter()
    .map(|(name, kind, id)| (EntryName::new(name).unwrap(), TreeEntry { kind, id }))
    .collect()
}

fn id<T: From<ObjectId>>(seed: &str) -> T {
    EncodedObject::blob(seed.as_bytes()).id().into()
}

fn sample_revision() -> Revision {
    Revision {
        change: ChangeId::from_bytes([7; 16]),
        predecessors: vec![id("old")],
        base: Base::Acceptance(id("main")),
        deps: vec![id("dep-1"), id("dep-2")],
        snapshot: TreeId::from(sample_tree().encode().id()),
        description: id("Add retries to fetch"),
        author: "user:alice".parse().unwrap(),
    }
}

fn sample_statement() -> Statement {
    Statement {
        subject: RecordId::Revision(sample_revision().encode().id().into()),
        kind: Kind::new("ci.test").unwrap(),
        verdict: Some(Verdict::Pass),
        issuer: "node:ci-1".parse().unwrap(),
        inputs: Some(Inputs {
            paths: vec![RepoPath::new("src").unwrap(), RepoPath::root()],
            definition: Digest::of(b"cargo test"),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external: false,
        }),
        payload: Some(id("log")),
        supersedes: None,
        promoted_from: Some(id("original")),
    }
}

/// A key that signs the same way in every run.
pub(crate) fn sample_key() -> NodeKey {
    NodeKey::from_bytes(&[42; 32])
}

pub(crate) fn sample_acceptance() -> Acceptance {
    AcceptanceBody {
        target: TargetName::MAIN.id(),
        seq: 1,
        prev: Some(id("genesis")),
        state: TreeId::from(sample_tree().encode().id()),
        landed: vec![
            Source::Revision(sample_revision().encode().id().into()),
            Source::Target(id("overhaul")),
        ],
        evidence: vec![sample_statement().encode().id().into()],
        policy: Some(id("policy")),
    }
    .sign(&sample_key())
}

fn samples() -> [Object; 7] {
    [
        Object::Blob(b"hello\n".to_vec()),
        Object::Blob(Vec::new()),
        Object::Tree(Tree::default()),
        Object::Tree(sample_tree()),
        Object::Revision(sample_revision()),
        Object::Statement(sample_statement()),
        Object::Acceptance(sample_acceptance()),
    ]
}

#[test]
fn objects_round_trip() {
    for object in samples() {
        let encoded = object.encode();
        assert_eq!(encoded.kind().unwrap(), object.kind());
        assert_eq!(encoded.decode().unwrap(), object);
        let verified = EncodedObject::verify(encoded.id(), encoded.bytes().to_vec()).unwrap();
        assert_eq!(verified, encoded);
    }
}

/// Ids are the storage and wire format: changing the encoding changes
/// every id, so it must never happen by accident.
#[test]
fn encoding_is_stable() {
    let ids = samples().map(|object| object.encode().id().short());
    assert_eq!(
        ids,
        [
            "60200926b98f",
            "48fc721fbbc1",
            "ab13bedf42e8",
            "d06087ffcfd2",
            "8783b1fca530",
            "b4f235b57ce9",
            "f0442b2f9f6c",
        ]
    );
}

#[test]
fn acceptances_verify_only_against_their_signer() {
    let acceptance = sample_acceptance();
    acceptance.verify(&sample_key().public()).unwrap();
    let other = NodeKey::from_bytes(&[1; 32]);
    assert!(acceptance.verify(&other.public()).is_err());
    let public = sample_key().public();
    assert_eq!(public.to_string().parse::<NodePublicKey>().unwrap(), public);
    assert!("ab".parse::<NodePublicKey>().is_err());

    // Any change to the body breaks the signature: here the sequence
    // number, right after the kind and the target id.
    let mut bytes = acceptance.encode().into_bytes();
    bytes[1 + 32] += 1;
    let tampered: Acceptance = EncodedObject::hash(bytes).decode_as().unwrap();
    assert_eq!(tampered.body().seq, 2);
    assert!(tampered.verify(&sample_key().public()).is_err());
}

#[test]
fn decode_rejects_non_canonical_fields() {
    let encoded = sample_statement().encode();
    let decode = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = encoded.bytes().to_vec();
        edit(&mut bytes);
        EncodedObject::hash(bytes).decode_as::<Statement>()
    };
    // The subject's tag is the byte after the kind.
    assert_eq!(
        decode(&|bytes| bytes[1] = 9),
        Err(DecodeError::UnknownTag {
            field: "RecordId",
            tag: 9
        })
    );
    // `external` is the last byte before the payload, supersedes and
    // the promoted statement.
    let external = encoded.bytes().len() - 2 * (1 + ObjectId::LEN) - 1 - 1;
    assert_eq!(
        decode(&|bytes| bytes[external] = 2),
        Err(DecodeError::UnknownTag {
            field: "bool",
            tag: 2
        })
    );
    assert_eq!(
        decode(&|bytes| bytes.push(0)),
        Err(DecodeError::TrailingBytes)
    );
    assert_eq!(
        decode(&|bytes| {
            bytes.pop();
        }),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn paths_decode_only_in_canonical_form() {
    let mut bytes = Vec::new();
    Reader::write_str("/src/", &mut bytes);
    assert_eq!(
        RepoPath::read(&mut Reader::new(&bytes)),
        Err(DecodeError::NonCanonical("path"))
    );
}

#[test]
fn blob_from_reader_matches_blob() {
    let content = b"some file content";
    let read = EncodedObject::read_blob(&content[..], content.len()).unwrap();
    assert_eq!(read, EncodedObject::blob(content));
    assert_eq!(read.blob_content().unwrap(), content);
}

#[test]
fn empty_encodings_are_errors() {
    let empty = EncodedObject::hash(Vec::new());
    assert_eq!(empty.decode(), Err(DecodeError::Truncated));
    assert_eq!(empty.blob_content(), Err(DecodeError::Truncated));
}

#[test]
fn verify_rejects_wrong_id() {
    let blob = EncodedObject::blob(b"a");
    let other = EncodedObject::blob(b"b");
    assert!(EncodedObject::verify(blob.id(), other.into_bytes()).is_err());
}

#[test]
fn typed_decoders_check_the_kind() {
    let blob = EncodedObject::blob(b"x");
    assert_eq!(
        blob.decode_as::<Tree>(),
        Err(DecodeError::WrongKind {
            expected: ObjectKind::Tree,
            actual: ObjectKind::Blob,
        })
    );
    let statement = sample_statement().encode();
    assert!(statement.decode_as::<Statement>().is_ok());
    assert!(statement.decode_as::<Revision>().is_err());
}
