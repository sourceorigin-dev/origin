//! The binary format in which objects are transferred.
//!
//! A pack is a sequence of entries, each
//! `id:[u8; 32] len:u64 bytes:[u8; len]`, where `bytes` is the object's
//! encoding. Decoding checks every object against its id, so a pack from an
//! untrusted peer is safe to read.
//!
//! Transfers are limited in size. A [`Packer`] splits objects, in order,
//! into packs of at most [`Pack::LIMIT`] bytes, so a push can take several
//! requests. A fetch response can stop short at a [`Part`], and the client
//! asks again for the rest.

use std::collections::HashSet;

use crate::object::{DecodeError, IdMismatch, Reader};
use crate::{EncodedObject, ObjectId};

/// A batch of objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pack {
    /// The objects, in the order they were added.
    pub objects: Vec<EncodedObject>,
}

impl Pack {
    const HEADER_LEN: usize = ObjectId::LEN + 8;

    /// The most bytes one encoded pack can hold in a request or a response. A
    /// pack always holds at least one object, so a larger object is sent alone.
    pub const LIMIT: usize = 64 << 20;

    /// Returns the number of bytes `object` takes in an encoded pack.
    fn encoded_len(object: &EncodedObject) -> usize {
        Self::HEADER_LEN + object.bytes().len()
    }

    /// Encodes the pack as bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = self.objects.iter().map(Self::encoded_len).sum();
        let mut out = Vec::with_capacity(len);
        for object in &self.objects {
            out.extend_from_slice(object.id().as_bytes());
            out.extend_from_slice(&(object.bytes().len() as u64).to_le_bytes());
            out.extend_from_slice(object.bytes());
        }
        out
    }

    /// Decodes a pack and checks each object against its id. An object sent
    /// twice is kept once.
    ///
    /// # Errors
    ///
    /// Fails if the pack is truncated or an object does not match its id.
    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        let mut reader = Reader::new(bytes);
        let mut objects = Vec::new();
        let mut seen = HashSet::new();
        while reader.remaining() > 0 {
            let id = ObjectId::from_bytes(reader.array()?);
            // A length past the address space is past the end of the pack.
            let len = usize::try_from(reader.u64()?).unwrap_or(usize::MAX);
            let object = EncodedObject::verify(id, reader.take(len)?.to_vec())?;
            if seen.insert(id) {
                objects.push(object);
            }
        }
        Ok(Self { objects })
    }
}

/// Splits objects into packs of at most a limit ([`Pack::LIMIT`] by
/// default), in the order they're added. Objects only depend on objects
/// added before them, so each pack can be sent as soon as it's full.
#[derive(Debug)]
pub struct Packer {
    limit: usize,
    pack: Pack,
    /// The encoded size of `pack`.
    len: usize,
}

impl Default for Packer {
    fn default() -> Self {
        Self::new(Pack::LIMIT)
    }
}

impl Packer {
    /// Creates a packer that fills packs up to `limit` bytes.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            pack: Pack::default(),
            len: 0,
        }
    }

    /// Returns whether the current pack has room for `objects`. An empty pack
    /// has room for anything.
    #[must_use]
    pub fn fits<'o>(&self, objects: impl IntoIterator<Item = &'o EncodedObject>) -> bool {
        let len: usize = objects.into_iter().map(Pack::encoded_len).sum();
        self.pack.objects.is_empty() || self.len + len <= self.limit
    }

    /// Adds `object` to the current pack. If the pack has no room for it,
    /// returns the full pack and starts the next one with the object.
    #[must_use = "a full pack is returned, not kept"]
    pub fn add(&mut self, object: EncodedObject) -> Option<Pack> {
        self.add_together(vec![object])
    }

    /// Adds `objects` to the current pack, keeping them together. If the pack
    /// has no room for all of them, returns the full pack and starts the next
    /// one with them.
    #[must_use = "a full pack is returned, not kept"]
    pub fn add_together(&mut self, objects: Vec<EncodedObject>) -> Option<Pack> {
        let full = (!self.fits(&objects)).then(|| {
            self.len = 0;
            std::mem::take(&mut self.pack)
        });
        self.extend(objects);
        full
    }

    /// Adds `objects` to the current pack without checking for room. Use it for
    /// objects that [`Packer::fits`] said there is room for.
    pub fn extend(&mut self, objects: impl IntoIterator<Item = EncodedObject>) {
        for object in objects {
            self.len += Pack::encoded_len(&object);
            self.pack.objects.push(object);
        }
    }

    /// Returns whether no object was added since the last full pack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pack.objects.is_empty()
    }

    /// Returns the current pack.
    #[must_use]
    pub fn finish(self) -> Pack {
        self.pack
    }
}

/// A fetch's answer. It may stop short of everything requested, to stay
/// within [`Pack::LIMIT`]. Then `more` is set, and asking again for the rest
/// returns the next part (see [`FetchRecords`](crate::api::FetchRecords) and
/// [`FetchSnapshot`](crate::api::FetchSnapshot)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Part {
    /// The objects of this part.
    pub pack: Pack,
    /// Whether the answer stopped short.
    pub more: bool,
}

/// Bytes that are not a valid pack.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack ended in the middle of an entry.
    #[error("cannot decode pack")]
    Malformed(#[from] DecodeError),
    /// An object does not match its id.
    #[error(transparent)]
    Corrupt(#[from] IdMismatch),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let pack = Pack {
            objects: vec![EncodedObject::blob(b"a"), EncodedObject::blob(b"")],
        };
        assert_eq!(Pack::decode(&pack.encode()).unwrap(), pack);
        assert_eq!(Pack::decode(&[]).unwrap(), Pack::default());
    }

    #[test]
    fn keeps_each_object_once() {
        let blob = EncodedObject::blob(b"a");
        let pack = Pack {
            objects: vec![blob.clone(), blob.clone()],
        };
        assert_eq!(Pack::decode(&pack.encode()).unwrap().objects, vec![blob]);
    }

    #[test]
    fn packers_split_in_order_within_the_limit() {
        let large = [b'x'; 100];
        let objects: Vec<_> = [b"a".as_slice(), b"bb", b"c", &large]
            .map(EncodedObject::blob)
            .to_vec();
        let limit = 2 * Pack::encoded_len(&objects[0]) + 1;
        let mut packer = Packer::new(limit);
        let mut packs: Vec<Pack> = objects
            .iter()
            .filter_map(|object| packer.add(object.clone()))
            .collect();
        packs.push(packer.finish());
        let split: Vec<usize> = packs.iter().map(|pack| pack.objects.len()).collect();
        assert_eq!(split, [2, 1, 1], "one larger than the limit travels alone");
        let joined: Vec<_> = packs.into_iter().flat_map(|pack| pack.objects).collect();
        assert_eq!(joined, objects, "in order");
    }

    #[test]
    fn rejects_truncated_and_corrupt_packs() {
        let mut bytes = Pack {
            objects: vec![EncodedObject::blob(b"content")],
        }
        .encode();
        assert!(matches!(
            Pack::decode(&bytes[..bytes.len() - 1]),
            Err(PackError::Malformed(DecodeError::Truncated))
        ));
        *bytes.last_mut().unwrap() ^= 1;
        assert!(matches!(Pack::decode(&bytes), Err(PackError::Corrupt(_))));
    }
}
