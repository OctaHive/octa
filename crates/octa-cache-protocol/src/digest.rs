//! Tagged content digests used by action and blob identities.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::CacheProtocolError;

const HASH_BYTES: usize = 32;

/// Cryptographic algorithm attached to digest bytes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestAlgorithm {
  /// Native content identity for Octa cache data.
  Blake3,
  /// External identity retained for plugins and OCI content.
  Sha256,
}

impl DigestAlgorithm {
  pub(crate) fn canonical_tag(self) -> u8 {
    match self {
      Self::Blake3 => 1,
      Self::Sha256 => 2,
    }
  }
}

impl fmt::Display for DigestAlgorithm {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(match self {
      Self::Blake3 => "blake3",
      Self::Sha256 => "sha256",
    })
  }
}

impl FromStr for DigestAlgorithm {
  type Err = CacheProtocolError;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "blake3" => Ok(Self::Blake3),
      "sha256" => Ok(Self::Sha256),
      _ => Err(CacheProtocolError::Digest(format!("unsupported algorithm '{value}'"))),
    }
  }
}

/// Algorithm-tagged 256-bit digest plus the byte size of its source object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest {
  algorithm: DigestAlgorithm,
  hash: [u8; HASH_BYTES],
  size_bytes: u64,
}

impl Digest {
  /// Creates a digest from validated binary parts.
  pub const fn new(algorithm: DigestAlgorithm, hash: [u8; HASH_BYTES], size_bytes: u64) -> Self {
    Self {
      algorithm,
      hash,
      size_bytes,
    }
  }

  /// Parses an exact lowercase 256-bit hexadecimal digest.
  pub fn from_hex(algorithm: DigestAlgorithm, hash: &str, size_bytes: u64) -> Result<Self, CacheProtocolError> {
    if hash.len() != HASH_BYTES * 2 {
      return Err(CacheProtocolError::Digest(
        "hash must contain exactly 64 lowercase hexadecimal characters".to_owned(),
      ));
    }
    let mut bytes = [0_u8; HASH_BYTES];
    let (pairs, remainder) = hash.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (index, pair) in pairs.iter().enumerate() {
      let high = decode_nibble(pair[0])?;
      let low = decode_nibble(pair[1])?;
      bytes[index] = high * 16 + low;
    }
    Ok(Self::new(algorithm, bytes, size_bytes))
  }

  /// Hashes an in-memory object using Octa's native cache algorithm.
  pub fn blake3(bytes: &[u8]) -> Self {
    Self::new(
      DigestAlgorithm::Blake3,
      *blake3::hash(bytes).as_bytes(),
      bytes.len() as u64,
    )
  }

  /// Returns the algorithm required to interpret the digest bytes.
  pub const fn algorithm(self) -> DigestAlgorithm {
    self.algorithm
  }

  /// Returns the exact 256-bit digest value.
  pub const fn bytes(self) -> [u8; HASH_BYTES] {
    self.hash
  }

  /// Returns the byte length of the object represented by this digest.
  pub const fn size_bytes(self) -> u64 {
    self.size_bytes
  }

  /// Returns the stable lowercase hexadecimal representation.
  pub fn hex(self) -> String {
    let mut result = String::with_capacity(HASH_BYTES * 2);
    for byte in self.hash {
      use std::fmt::Write as _;
      write!(&mut result, "{byte:02x}").expect("writing into a String cannot fail");
    }
    result
  }
}

impl fmt::Display for Digest {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}:{}:{}", self.algorithm, self.hex(), self.size_bytes)
  }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DigestWire {
  algorithm: String,
  hash: String,
  size_bytes: u64,
}

impl Serialize for Digest {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    DigestWire {
      algorithm: self.algorithm.to_string(),
      hash: self.hex(),
      size_bytes: self.size_bytes,
    }
    .serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for Digest {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let wire = DigestWire::deserialize(deserializer)?;
    let algorithm = DigestAlgorithm::from_str(&wire.algorithm).map_err(serde::de::Error::custom)?;
    Self::from_hex(algorithm, &wire.hash, wire.size_bytes).map_err(serde::de::Error::custom)
  }
}

fn decode_nibble(byte: u8) -> Result<u8, CacheProtocolError> {
  match byte {
    b'0'..=b'9' => Ok(byte - b'0'),
    b'a'..=b'f' => Ok(byte - b'a' + 10),
    _ => Err(CacheProtocolError::Digest(
      "hash must use lowercase hexadecimal characters".to_owned(),
    )),
  }
}
