//! The object model: blobs, trees and the records built on them, and their
//! canonical encoding.
//!
//! Every object has exactly one encoding, and its id is the BLAKE3 hash of
//! that encoding. The first byte is the [`ObjectKind`], so objects of
//! different kinds never share an id. The design's "Records" section says what
//! each kind holds and how its fields are encoded; for a tree, see [`Tree`].

use std::fmt;
use std::io::{self, Read};

use crate::ObjectId;

pub use acceptance::{
    Acceptance, AcceptanceBody, BadSignature, NodeKey, NodePublicKey, Signature, Source,
};
pub use encoding::Reader;
pub(crate) use encoding::{Field, fields, tagged};
pub use errors::{DecodeError, IdMismatch};
pub use revision::{Base, Revision};
pub use statement::{InputSource, Inputs, Kind, RecordId, Statement, Verdict};
pub use tree::{EntryKind, EntryName, Tree, TreeEntry};

mod acceptance;
mod encoding;
mod errors;
mod revision;
mod statement;
mod tree;

/// The kind of an object, stored as the first byte of its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// A file's content.
    Blob = 1,
    /// A directory listing.
    Tree = 2,
    /// One version of a change.
    Revision = 3,
    /// A claim about another record.
    Statement = 4,
    /// One landing on a target.
    Acceptance = 5,
    /// A revision's description or a statement's payload.
    Attachment = 6,
}

impl ObjectKind {
    fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        match byte {
            1 => Ok(Self::Blob),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Revision),
            4 => Ok(Self::Statement),
            5 => Ok(Self::Acceptance),
            6 => Ok(Self::Attachment),
            _ => Err(DecodeError::UnknownObjectKind(byte)),
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Revision => "revision",
            Self::Statement => "statement",
            Self::Acceptance => "acceptance",
            Self::Attachment => "attachment",
        })
    }
}

/// An object with structure: every kind except blobs and attachments.
pub trait Record: Field {
    /// The kind every encoding of this record starts with.
    const KIND: ObjectKind;

    /// Encodes and hashes the record.
    fn encode(&self) -> EncodedObject {
        let mut bytes = vec![Self::KIND as u8];
        self.write(&mut bytes);
        EncodedObject::hash(bytes)
    }
}

/// Defines [`Object`], with a variant for each record type.
macro_rules! object {
    ($($record:ident),*) => {
        /// A decoded object.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum Object {
            /// A file's content.
            Blob(Vec<u8>),
            /// A revision's description or a statement's payload.
            Attachment(Vec<u8>),
            $(#[doc = concat!("A ", stringify!($record), ".")] $record($record),)*
        }

        impl Object {
            /// The kind of this object.
            #[must_use]
            pub fn kind(&self) -> ObjectKind {
                match self {
                    Self::Blob(_) => ObjectKind::Blob,
                    Self::Attachment(_) => ObjectKind::Attachment,
                    $(Self::$record(_) => $record::KIND,)*
                }
            }

            /// Encodes and hashes the object.
            #[must_use]
            pub fn encode(&self) -> EncodedObject {
                match self {
                    Self::Blob(content) => EncodedObject::blob(content),
                    Self::Attachment(content) => EncodedObject::attachment(content),
                    $(Self::$record(record) => record.encode(),)*
                }
            }
        }

        impl EncodedObject {
            /// Decodes the object.
            ///
            /// # Errors
            ///
            /// Fails if the bytes are not a canonical encoding.
            pub fn decode(&self) -> Result<Object, DecodeError> {
                match self.kind()? {
                    ObjectKind::Blob => Ok(Object::Blob(self.bytes[1..].to_vec())),
                    ObjectKind::Attachment => Ok(Object::Attachment(self.bytes[1..].to_vec())),
                    $(ObjectKind::$record => self.decode_as().map(Object::$record),)*
                }
            }
        }

        $(impl From<$record> for Object {
            fn from(record: $record) -> Self {
                Self::$record(record)
            }
        })*
    };
}

object!(Tree, Revision, Statement, Acceptance);

/// An object's encoding together with its id.
///
/// The id is always the hash of the bytes: constructors either hash the bytes
/// or verify a claimed id, so holding an `EncodedObject` proves integrity.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedObject {
    id: ObjectId,
    bytes: Vec<u8>,
}

impl EncodedObject {
    fn hash(bytes: Vec<u8>) -> Self {
        Self {
            id: ObjectId::hash(&bytes),
            bytes,
        }
    }

    /// Encodes a blob, reading its content to the end. `len_hint` sizes the
    /// buffer up front, so passing the file size avoids reallocating.
    ///
    /// # Errors
    ///
    /// Fails if reading fails.
    pub fn read_blob(reader: impl Read, len_hint: usize) -> io::Result<Self> {
        Self::read_raw(ObjectKind::Blob, reader, len_hint)
    }

    /// Encodes an attachment, reading its content to the end, like
    /// [`EncodedObject::read_blob`].
    ///
    /// # Errors
    ///
    /// Fails if reading fails.
    pub fn read_attachment(reader: impl Read, len_hint: usize) -> io::Result<Self> {
        Self::read_raw(ObjectKind::Attachment, reader, len_hint)
    }

    /// Encodes content read to the end, as is, as an object of `kind`.
    fn read_raw(kind: ObjectKind, mut reader: impl Read, len_hint: usize) -> io::Result<Self> {
        let mut bytes = Vec::with_capacity(len_hint + 1);
        bytes.push(kind as u8);
        reader.read_to_end(&mut bytes)?;
        Ok(Self::hash(bytes))
    }

    /// Encodes a blob from its content.
    #[must_use]
    pub fn blob(content: &[u8]) -> Self {
        Self::raw(ObjectKind::Blob, content)
    }

    /// Encodes an attachment from its content.
    #[must_use]
    pub fn attachment(content: &[u8]) -> Self {
        Self::raw(ObjectKind::Attachment, content)
    }

    /// Encodes content, as is, as an object of `kind`.
    fn raw(kind: ObjectKind, content: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(content.len() + 1);
        bytes.push(kind as u8);
        bytes.extend_from_slice(content);
        Self::hash(bytes)
    }

    /// Accepts bytes claimed to encode the object `id`, such as bytes read
    /// from disk or the network.
    ///
    /// # Errors
    ///
    /// Fails if the bytes do not hash to `id`.
    pub fn verify(id: ObjectId, bytes: Vec<u8>) -> Result<Self, IdMismatch> {
        let actual = ObjectId::hash(&bytes);
        if actual == id {
            Ok(Self { id, bytes })
        } else {
            Err(IdMismatch {
                expected: id,
                actual,
            })
        }
    }

    /// The id of the object.
    #[must_use]
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The canonical encoding.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The kind of the object.
    ///
    /// # Errors
    ///
    /// Fails if the encoding does not start with a known kind.
    pub fn kind(&self) -> Result<ObjectKind, DecodeError> {
        ObjectKind::from_byte(*self.bytes.first().ok_or(DecodeError::Truncated)?)
    }

    /// A blob's content, without copying it.
    ///
    /// # Errors
    ///
    /// Fails if the object is not a blob.
    pub fn blob_content(&self) -> Result<&[u8], DecodeError> {
        self.content(ObjectKind::Blob)
    }

    /// Returns an attachment's content, without copying it.
    ///
    /// # Errors
    ///
    /// Fails if the object is not an attachment. A blob is never read as one.
    pub fn attachment_content(&self) -> Result<&[u8], DecodeError> {
        self.content(ObjectKind::Attachment)
    }

    /// Returns the content of an object of `kind` that is stored as is.
    fn content(&self, kind: ObjectKind) -> Result<&[u8], DecodeError> {
        self.expect_kind(kind)?;
        Ok(&self.bytes[1..])
    }

    /// Decodes the object as a `R`.
    ///
    /// # Errors
    ///
    /// Fails if it is another kind of object or is not a canonical encoding.
    pub fn decode_as<R: Record>(&self) -> Result<R, DecodeError> {
        self.expect_kind(R::KIND)?;
        let mut reader = Reader::new(&self.bytes[1..]);
        let record = R::read(&mut reader)?;
        reader.finish()?;
        Ok(record)
    }

    fn expect_kind(&self, expected: ObjectKind) -> Result<(), DecodeError> {
        let actual = self.kind()?;
        if actual == expected {
            Ok(())
        } else {
            Err(DecodeError::WrongKind { expected, actual })
        }
    }

    /// Consumes the object, returning its encoding.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for EncodedObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedObject")
            .field("id", &self.id)
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod tests;
