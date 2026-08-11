//! Canonical encoding: the deterministic CBOR subset that request values are
//! hashed through.
//!
//! A memo table is only as trustworthy as the equality it uses. Two requests
//! that mean the same thing must hash the same, and two that mean different
//! things must never hash the same, or a cache hit hands back somebody else's
//! answer. That is an injectivity requirement on the encoder, not a style
//! preference, so the subset is chosen to make injectivity provable rather
//! than tested for.
//!
//! # The subset
//!
//! * **Definite lengths only.** No indefinite-length strings, arrays or maps,
//!   so every item's extent is known from its head.
//! * **Minimal-width integers.** A value is encoded in the shortest of the
//!   five widths that holds it, so each integer has exactly one encoding.
//! * **Map keys sorted bytewise by their encoded form**, and duplicate keys
//!   rejected. Sorting is over the encoded key bytes (RFC 8949 §4.2.1), not
//!   over any in-memory ordering, so the order does not depend on Rust's
//!   `Ord` impls.
//! * **No floats.** They are excluded at the type level: [`CanonValue`] has no
//!   float variant, so a float cannot be encoded rather than being encoded and
//!   then rejected. This is stronger than a runtime check and is why no
//!   `FloatRejected` error exists.
//! * **Strings are UTF-8 and assumed NFC.** Rust's `String` gives UTF-8;
//!   normalisation is *not* performed here, because pulling in a Unicode
//!   normalisation table is a dependency decision for the layer that owns
//!   user-facing text. Callers must hand us NFC. A non-NFC string encodes
//!   fine and simply hashes as a different request, which is a cache miss, not
//!   a wrong answer.
//!
//! # Why this is injective
//!
//! 1. Every item starts with a head byte whose major type names the variant,
//!    so items of different kinds cannot share an encoding.
//! 2. Definite lengths make decoding a single left-to-right walk with no
//!    lookahead and no ambiguity about where an item ends.
//! 3. Minimal-width integers, and the fixed one-byte encodings of null and the
//!    booleans, give each scalar exactly one form.
//! 4. Sorted, duplicate-free map entries give each map exactly one form.
//!
//! So `encode` is total and injective on [`CanonValue`]: distinct values
//! produce distinct byte strings. The [`VERSION`] tag rides along in the
//! domain (see [`crate::Domain::mint`]) rather than in the bytes, so a future
//! `canon-v2` mints different domains instead of colliding with v1 rows in
//! anybody's existing lock file.

use core::fmt;

/// Participates in hashing through the domain, not through the encoded bytes.
pub const VERSION: &str = "canon-v1";

/// A value in the canonical subset. Deliberately smaller than CBOR: no
/// floats, no tags, no simple values beyond `null` and the booleans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonValue {
    Null,
    Bool(bool),
    /// Covers the whole CBOR integer range, which is wider than `i64` in both
    /// directions: unsigned reaches `2^64 - 1` and negative reaches `-2^64`.
    Int(i128),
    Bytes(Vec<u8>),
    Str(String),
    Array(Vec<CanonValue>),
    /// Held as pairs rather than a `BTreeMap` because the canonical order is
    /// over *encoded* keys, which no Rust `Ord` impl reproduces. The encoder
    /// sorts, so construction order is free and never observable.
    Map(Vec<(CanonValue, CanonValue)>),
}

impl CanonValue {
    /// Build a map from string keys, the shape almost every request has.
    #[must_use]
    pub fn map<I, K>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, Self)>,
        K: Into<String>,
    {
        Self::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Self::Str(key.into()), value))
                .collect(),
        )
    }

    /// Build an array.
    #[must_use]
    pub fn array<I: IntoIterator<Item = Self>>(items: I) -> Self {
        Self::Array(items.into_iter().collect())
    }

    /// Build a string.
    #[must_use]
    pub fn str(text: impl Into<String>) -> Self {
        Self::Str(text.into())
    }

    /// Build an integer from anything that fits.
    #[must_use]
    pub fn int(value: impl Into<i128>) -> Self {
        Self::Int(value.into())
    }
}

/// Why a value could not be canonically encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonError {
    /// Outside the CBOR integer range: `-2^64 ..= 2^64 - 1`.
    IntOutOfRange { value: i128 },
    /// Two entries in one map encoded to the same key bytes. Accepting this
    /// would make the map's encoding depend on which duplicate won.
    DuplicateKey { key_hex: String },
    /// The value nests deeper than [`MAX_DEPTH`]. A depth cap keeps encoding
    /// non-recursive in the pathological case and bounds an attacker's ability
    /// to blow the stack with a deeply nested request.
    TooDeep { limit: u32 },
}

impl fmt::Display for CanonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntOutOfRange { value } => {
                write!(f, "integer {value} is outside the CBOR range -2^64 ..= 2^64-1")
            }
            Self::DuplicateKey { key_hex } => {
                write!(f, "duplicate map key, encoded as {key_hex}")
            }
            Self::TooDeep { limit } => write!(f, "value nests deeper than {limit} levels"),
        }
    }
}

impl core::error::Error for CanonError {}

/// Deepest nesting the encoder will follow.
pub const MAX_DEPTH: u32 = 128;

/// Largest CBOR unsigned integer, `2^64 - 1`.
const MAX_UINT: i128 = u64::MAX as i128;
/// Smallest CBOR negative integer, `-2^64`. The negative line reaches one
/// further than the positive one because major 1 holds `-1 - n`.
const MIN_NEGINT: i128 = -1 - MAX_UINT;

/// Encode a value into the canonical subset.
pub fn encode(value: &CanonValue) -> Result<Vec<u8>, CanonError> {
    let mut out = Vec::new();
    write_value(value, 0, &mut out)?;
    Ok(out)
}

// CBOR major types, shifted into the top three bits of the head byte.
const MAJOR_UINT: u8 = 0 << 5;
const MAJOR_NEGINT: u8 = 1 << 5;
const MAJOR_BYTES: u8 = 2 << 5;
const MAJOR_TEXT: u8 = 3 << 5;
const MAJOR_ARRAY: u8 = 4 << 5;
const MAJOR_MAP: u8 = 5 << 5;
const MAJOR_SIMPLE: u8 = 7 << 5;

const SIMPLE_FALSE: u8 = 20;
const SIMPLE_TRUE: u8 = 21;
const SIMPLE_NULL: u8 = 22;

fn write_value(value: &CanonValue, depth: u32, out: &mut Vec<u8>) -> Result<(), CanonError> {
    if depth > MAX_DEPTH {
        return Err(CanonError::TooDeep { limit: MAX_DEPTH });
    }
    match value {
        CanonValue::Null => out.push(MAJOR_SIMPLE | SIMPLE_NULL),
        CanonValue::Bool(false) => out.push(MAJOR_SIMPLE | SIMPLE_FALSE),
        CanonValue::Bool(true) => out.push(MAJOR_SIMPLE | SIMPLE_TRUE),
        CanonValue::Int(n) => write_int(*n, out)?,
        CanonValue::Bytes(bytes) => {
            write_head(MAJOR_BYTES, bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        CanonValue::Str(text) => {
            write_head(MAJOR_TEXT, text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        CanonValue::Array(items) => {
            write_head(MAJOR_ARRAY, items.len() as u64, out);
            for item in items {
                write_value(item, depth + 1, out)?;
            }
        }
        CanonValue::Map(entries) => write_map(entries, depth, out)?,
    }
    Ok(())
}

fn write_map(
    entries: &[(CanonValue, CanonValue)],
    depth: u32,
    out: &mut Vec<u8>,
) -> Result<(), CanonError> {
    // Encode each key first: the canonical order is over encoded key bytes,
    // so it cannot be decided without encoding.
    let mut encoded = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let mut key_bytes = Vec::new();
        write_value(key, depth + 1, &mut key_bytes)?;
        encoded.push((key_bytes, value));
    }
    encoded.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(duplicate) = first_duplicate(&encoded) {
        return Err(CanonError::DuplicateKey {
            key_hex: duplicate.iter().map(|b| format!("{b:02x}")).collect(),
        });
    }

    write_head(MAJOR_MAP, encoded.len() as u64, out);
    for (key_bytes, value) in &encoded {
        out.extend_from_slice(key_bytes);
        write_value(value, depth + 1, out)?;
    }
    Ok(())
}

fn first_duplicate<'a>(encoded: &'a [(Vec<u8>, &CanonValue)]) -> Option<&'a [u8]> {
    encoded
        .windows(2)
        .find_map(|pair| match (pair.first(), pair.last()) {
            (Some(left), Some(right)) if left.0 == right.0 => Some(left.0.as_slice()),
            _ => None,
        })
}

fn write_int(value: i128, out: &mut Vec<u8>) -> Result<(), CanonError> {
    // CBOR splits the integer line at zero: non-negative values are major 0
    // holding n, negative values are major 1 holding -1 - n, which is why the
    // negative range reaches one further than the positive one.
    if !(MIN_NEGINT..=MAX_UINT).contains(&value) {
        return Err(CanonError::IntOutOfRange { value });
    }
    // Range-checked above, so neither the negation nor the narrowing can trap.
    let (major, magnitude) = if value >= 0 {
        (MAJOR_UINT, value)
    } else {
        (MAJOR_NEGINT, -1 - value)
    };
    let magnitude = u64::try_from(magnitude).map_err(|_| CanonError::IntOutOfRange { value })?;
    write_head(major, magnitude, out);
    Ok(())
}

/// Write a head byte plus the shortest argument encoding that holds `value`.
fn write_head(major: u8, value: u64, out: &mut Vec<u8>) {
    match value {
        // Values below 24 live in the head byte itself; 24..=27 are the
        // reserved width markers, which is why the inline range stops at 23.
        0..=23 => out.push(major | (value as u8)),
        24..=0xff => {
            out.push(major | 24);
            out.push(value as u8);
        }
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn scalars_match_rfc_8949_examples() -> Result<(), CanonError> {
        assert_eq!(hex(&encode(&CanonValue::Null)?), "f6");
        assert_eq!(hex(&encode(&CanonValue::Bool(false))?), "f4");
        assert_eq!(hex(&encode(&CanonValue::Bool(true))?), "f5");
        assert_eq!(hex(&encode(&CanonValue::Int(0))?), "00");
        assert_eq!(hex(&encode(&CanonValue::Int(23))?), "17");
        assert_eq!(hex(&encode(&CanonValue::Int(24))?), "1818");
        assert_eq!(hex(&encode(&CanonValue::Int(1000))?), "1903e8");
        assert_eq!(hex(&encode(&CanonValue::Int(-1))?), "20");
        assert_eq!(hex(&encode(&CanonValue::Int(-500))?), "3901f3");
        assert_eq!(hex(&encode(&CanonValue::str("a"))?), "6161");
        assert_eq!(hex(&encode(&CanonValue::Bytes(vec![1, 2]))?), "420102");
        Ok(())
    }

    #[test]
    fn integers_use_the_shortest_width() -> Result<(), CanonError> {
        // One byte of head per step up, and no wasted width in between.
        for (value, len) in [(23i128, 1), (24, 2), (0xff, 2), (0x100, 3), (0x1_0000, 5)] {
            assert_eq!(encode(&CanonValue::Int(value))?.len(), len, "value {value}");
        }
        Ok(())
    }

    #[test]
    fn integer_range_is_the_full_cbor_line() {
        let top = i128::from(u64::MAX);
        assert!(encode(&CanonValue::Int(top)).is_ok());
        assert!(encode(&CanonValue::Int(-1 - top)).is_ok());
        assert_eq!(
            encode(&CanonValue::Int(top + 1)),
            Err(CanonError::IntOutOfRange { value: top + 1 })
        );
    }

    #[test]
    fn map_order_does_not_depend_on_construction_order() -> Result<(), CanonError> {
        let one = CanonValue::map([("b", CanonValue::Int(2)), ("a", CanonValue::Int(1))]);
        let other = CanonValue::map([("a", CanonValue::Int(1)), ("b", CanonValue::Int(2))]);
        assert_eq!(encode(&one)?, encode(&other)?);
        assert_eq!(hex(&encode(&one)?), "a2616101616202");
        Ok(())
    }

    /// Bytewise order over encoded keys, not lexicographic order over the
    /// decoded strings: "z" sorts before "aa" because its encoding is shorter
    /// and CBOR heads make length the leading byte.
    #[test]
    fn short_keys_sort_before_long_ones() -> Result<(), CanonError> {
        let encoded = encode(&CanonValue::map([
            ("aa", CanonValue::Null),
            ("z", CanonValue::Null),
        ]))?;
        assert_eq!(hex(&encoded), "a2617af6626161f6");
        Ok(())
    }

    #[test]
    fn duplicate_keys_are_refused() {
        let value = CanonValue::map([("a", CanonValue::Int(1)), ("a", CanonValue::Int(2))]);
        assert!(matches!(
            encode(&value),
            Err(CanonError::DuplicateKey { .. })
        ));
    }

    /// The property the memo table depends on. Distinct values that a sloppier
    /// encoder would confuse must stay distinct here.
    #[test]
    fn structurally_distinct_values_do_not_collide() -> Result<(), CanonError> {
        let candidates = [
            CanonValue::Null,
            CanonValue::Bool(false),
            CanonValue::Int(0),
            CanonValue::Bytes(vec![0x61]),
            CanonValue::str("a"),
            CanonValue::array([CanonValue::str("a")]),
            CanonValue::map([("a", CanonValue::Null)]),
            CanonValue::array([CanonValue::str("ab")]),
            CanonValue::array([CanonValue::str("a"), CanonValue::str("b")]),
        ];
        let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        for candidate in &candidates {
            assert!(seen.insert(encode(candidate)?), "collision on {candidate:?}");
        }
        Ok(())
    }

    #[test]
    fn depth_is_capped() {
        let mut value = CanonValue::Null;
        for _ in 0..=MAX_DEPTH {
            value = CanonValue::array([value]);
        }
        assert_eq!(encode(&value), Err(CanonError::TooDeep { limit: MAX_DEPTH }));
    }
}
