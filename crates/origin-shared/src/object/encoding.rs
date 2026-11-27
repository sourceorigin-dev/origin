//! The building blocks of record encodings. Each field type has exactly
//! one encoding, so each record has exactly one too.

use super::DecodeError;

/// A value with a canonical, self-delimiting binary encoding.
///
/// The trait is public only so the crate's public traits can require it.
/// Code outside the crate can't reach it, so encodings stay internal.
pub trait Field: Sized {
    /// Appends the encoding to `out`.
    fn write(&self, out: &mut Vec<u8>);

    /// Decodes a value. Rejects any encoding that [`Field::write`] would not
    /// produce.
    ///
    /// # Errors
    ///
    /// Fails if the bytes are not a canonical encoding.
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError>;
}

/// Implements [`Field`] for a struct by encoding its fields in order.
macro_rules! fields {
    ($type:ident { $($field:ident),* $(,)? }) => {
        impl $crate::object::Field for $type {
            fn write(&self, out: &mut Vec<u8>) {
                $($crate::object::Field::write(&self.$field, out);)*
            }

            fn read(
                reader: &mut $crate::object::Reader<'_>,
            ) -> Result<Self, $crate::object::DecodeError> {
                Ok(Self {
                    $($field: $crate::object::Field::read(reader)?,)*
                })
            }
        }
    };
}

/// Implements [`Field`] for an enum whose variants each hold one field, as a
/// tag byte followed by the field.
macro_rules! tagged {
    ($type:ident { $($variant:ident = $tag:literal),* $(,)? }) => {
        impl $crate::object::Field for $type {
            fn write(&self, out: &mut Vec<u8>) {
                match self {
                    $(Self::$variant(value) => {
                        out.push($tag);
                        $crate::object::Field::write(value, out);
                    })*
                }
            }

            fn read(
                reader: &mut $crate::object::Reader<'_>,
            ) -> Result<Self, $crate::object::DecodeError> {
                match reader.u8()? {
                    $($tag => Ok(Self::$variant($crate::object::Field::read(reader)?)),)*
                    tag => Err($crate::object::DecodeError::UnknownTag {
                        field: stringify!($type),
                        tag,
                    }),
                }
            }
        }
    };
}

pub(crate) use {fields, tagged};

/// Encoded as 8 bytes, little endian.
impl Field for u64 {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        reader.u64()
    }
}

/// Encoded as one byte, 0 or 1.
impl Field for bool {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(DecodeError::UnknownTag { field: "bool", tag }),
        }
    }
}

/// Encoded as a `u32` count, then the items.
impl<T: Field> Field for Vec<T> {
    fn write(&self, out: &mut Vec<u8>) {
        Reader::write_len(self.len(), out);
        for item in self {
            item.write(out);
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let len = reader.read_len()?;
        // Each item takes at least a byte: a corrupt count can't make this
        // allocate more than the encoding's size.
        let mut items = Vec::with_capacity(len.min(reader.remaining()));
        for _ in 0..len {
            items.push(T::read(reader)?);
        }
        Ok(items)
    }
}

/// Encoded as a 0 byte for `None`, or a 1 byte and the value.
impl<T: Field> Field for Option<T> {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(value) => {
                out.push(1);
                value.write(out);
            }
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        reader.option(T::read)
    }
}

/// A cursor over an encoding. Reading past the end fails with
/// [`DecodeError::Truncated`] instead of panicking.
///
/// Records are read with it. Other crates read their own binary formats
/// with it too, with the same conventions: integers are little endian and
/// lengths are `u32`s.
#[derive(Debug)]
pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// A cursor at the start of `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    /// Writes a length as a little-endian `u32`. Panics past 4 GiB, which
    /// nothing in a record comes close to.
    #[expect(
        clippy::expect_used,
        reason = "Nothing in a record comes close to 4 GiB."
    )]
    pub(crate) fn write_len(len: usize, out: &mut Vec<u8>) {
        let len = u32::try_from(len).expect("record field longer than 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
    }

    /// Writes a string as a `u32` length and its bytes.
    pub(crate) fn write_str(string: &str, out: &mut Vec<u8>) {
        Self::write_len(string.len(), out);
        out.extend_from_slice(string.as_bytes());
    }

    /// Reads a length, encoded as a `u32`.
    ///
    /// # Errors
    ///
    /// Fails if the bytes end first.
    pub fn read_len(&mut self) -> Result<usize, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?) as usize)
    }

    /// Reads the next `len` bytes.
    ///
    /// # Errors
    ///
    /// Fails if fewer remain.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let (head, rest) = self.0.split_at_checked(len).ok_or(DecodeError::Truncated)?;
        self.0 = rest;
        Ok(head)
    }

    /// Reads the next `N` bytes.
    ///
    /// # Errors
    ///
    /// Fails if fewer remain.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        // `take` returns exactly `N` bytes: this never fails.
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    /// Reads a byte.
    ///
    /// # Errors
    ///
    /// Fails if none remains.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a `u64`.
    ///
    /// # Errors
    ///
    /// Fails if the bytes end first.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_le_bytes)
    }

    /// Reads bytes preceded by their `u32` length.
    ///
    /// # Errors
    ///
    /// Fails if the bytes end first.
    pub fn prefixed(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.read_len()?;
        self.take(len)
    }

    /// Reads an option: a 0 byte for `None`, or a 1 byte and the value that
    /// `read` reads.
    ///
    /// # Errors
    ///
    /// Fails if the tag byte is neither 0 nor 1, or if `read` fails.
    pub fn option<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Option<T>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => read(self).map(Some),
            tag => Err(DecodeError::UnknownTag {
                field: "option",
                tag,
            }),
        }
    }

    pub(crate) fn str_until_nul(&mut self) -> Result<&'a str, DecodeError> {
        let len = self
            .0
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(DecodeError::Truncated)?;
        let name = self.utf8(len)?;
        self.take(1)?;
        Ok(name)
    }

    /// Reads a string: UTF-8 bytes preceded by their `u32` length.
    ///
    /// # Errors
    ///
    /// Fails if the bytes end first or aren't UTF-8.
    pub fn prefixed_str(&mut self) -> Result<&'a str, DecodeError> {
        let len = self.read_len()?;
        self.utf8(len)
    }

    fn utf8(&mut self, len: usize) -> Result<&'a str, DecodeError> {
        str::from_utf8(self.take(len)?).map_err(|_| DecodeError::InvalidUtf8)
    }

    /// How many bytes are left to read.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.0.len()
    }

    /// Ends reading and checks that no bytes are left.
    ///
    /// # Errors
    ///
    /// Fails with [`DecodeError::TrailingBytes`] if bytes remain.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}
