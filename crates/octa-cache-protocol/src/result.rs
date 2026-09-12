//! Versioned metadata for replaying a successful cached task.
//!
//! An action digest answers *which execution is this?*; an action result answers
//! *what must Octa restore to reproduce that execution's observable result?*
//! The cache therefore stores two related, but deliberately separate, objects:
//!
//! ```text
//! ActionDescriptorV1 --canonical digest--> action key
//!                                             |
//!                                             v
//!                                      ActionResultV1
//!                                      |            |
//!                                      |            `- logical outputs,
//!                                      |               artifacts and reports
//!                                      v
//!                                immutable output bundle
//! ```
//!
//! [`ActionResultV1`] is small metadata indexed by the action key. Filesystem
//! contents live in a content-addressed bundle instead of being embedded in the
//! result. This keeps cache lookup cheap, allows the blob to be verified and
//! transferred independently, and permits storage implementations to deduplicate
//! an identical bundle referenced by different actions.
//!
//! # Publication and restore
//!
//! After a successful cacheable task, Octa captures only the declared output
//! roots into a canonical uncompressed byte stream. It computes the bundle
//! digest over those canonical bytes, optionally compresses them for transport,
//! stores the blob first, and only then publishes the action result. Both objects
//! are immutable and use create-if-absent writes; a different result already
//! published for the same action is nondeterminism, not an update to overwrite.
//!
//! On a hit, Octa validates this metadata, downloads the referenced encoding,
//! enforces transfer and expansion limits, verifies the uncompressed bytes
//! against the bundle digest, extracts into staging, and atomically replaces the
//! declared outputs. Only after verified restoration may it replay public task
//! outputs and artifact/report registrations. A malformed record, missing blob,
//! digest mismatch, or incomplete restore is a cache failure, never a successful
//! hit.
//!
//! This protocol crate defines portable metadata and its intrinsic invariants;
//! it does not read bundles or trust a descriptor merely because [`validate`](ActionResultV1::validate)
//! succeeds. The cache implementation must still enforce configured byte, entry,
//! path, compression-ratio, and output-contract limits while reading untrusted
//! storage.

use std::{
  collections::BTreeMap,
  io::{self, Write},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
  CacheProtocolError, Digest, DigestAlgorithm, RelativePath, MAX_ACTION_RESULT_METADATA_BYTES,
  MAX_ACTION_RESULT_WIRE_BYTES, MAX_CACHE_LIST_ITEMS, MAX_CACHE_STRING_BYTES,
};

/// Action-result wire format understood by this crate version.
///
/// This version is independent of the action-key format: one governs how an
/// execution is identified, while the other governs how its result is represented.
pub const ACTION_RESULT_VERSION_V1: u16 = 1;

/// Maximum Zstandard back-reference window accepted by `ZstdV1` decoders.
///
/// The value denotes `2^27` bytes (128 MiB). It is part of the physical format
/// contract: bounding expanded output alone is insufficient because a hostile
/// frame can request a large decoder window before yielding any bytes.
pub const ZSTD_V1_MAX_WINDOW_LOG: u32 = 27;

/// Physical encoding used to store or transfer one semantic bundle.
///
/// Encoding is intentionally absent from content identity. Recompressing the
/// same canonical bundle may change transfer bytes and size without changing
/// which filesystem result the blob represents.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobEncoding {
  /// Canonical bytes are stored without compression.
  Identity,
  /// Canonical bytes are stored as a Zstandard version-one stream bounded by
  /// [`ZSTD_V1_MAX_WINDOW_LOG`].
  ZstdV1,
}

/// Identity and wire-size metadata for an immutable output bundle.
///
/// The digest and expanded size describe the canonical uncompressed bundle.
/// The encoding and encoded size describe the physical object fetched from
/// storage. Keeping both views lets a reader reject truncation and excessive
/// expansion before accepting any restored files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlobDescriptor {
  /// Digest of the canonical uncompressed representation.
  pub digest: Digest,
  /// Physical transfer encoding; it does not participate in semantic identity.
  pub encoding: BlobEncoding,
  /// Bytes stored or transferred after encoding.
  pub encoded_size_bytes: u64,
  /// Bytes expected after decoding.
  pub expanded_size_bytes: u64,
  /// Number of filesystem entries in the canonical bundle.
  ///
  /// A decoder uses this as an expected value and as an early resource bound;
  /// it must still count the entries actually decoded.
  pub entry_count: u64,
}

impl BlobDescriptor {
  /// Validates format-level identity and size relationships.
  ///
  /// This does not validate blob bytes or apply deployment-specific maxima.
  /// Readers must compare the number of transferred bytes, decoded bytes,
  /// decoded entries, and final digest with this descriptor while streaming.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    if self.digest.algorithm() != DigestAlgorithm::Blake3 {
      return Err(CacheProtocolError::Result(
        "output bundle must use the native BLAKE3 digest".to_owned(),
      ));
    }
    if self.digest.size_bytes() != self.expanded_size_bytes {
      return Err(CacheProtocolError::Result(
        "output bundle digest size differs from expanded size".to_owned(),
      ));
    }
    if self.encoded_size_bytes == 0 || self.expanded_size_bytes == 0 || self.entry_count == 0 {
      return Err(CacheProtocolError::Result(
        "output bundle sizes and entry count must be greater than zero".to_owned(),
      ));
    }
    if self.encoding == BlobEncoding::Identity && self.encoded_size_bytes != self.expanded_size_bytes {
      return Err(CacheProtocolError::Result(
        "identity-encoded bundle sizes must match".to_owned(),
      ));
    }
    Ok(())
  }
}

/// Artifact registration reproduced after a cache hit.
///
/// This is only logical metadata. The referenced path must be captured inside
/// the output bundle; publication and restore code enforce that relationship
/// against the task's declared output roots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CachedArtifact {
  /// User-visible artifact registration name.
  pub name: String,
  /// Existing file or directory relative to the restored workspace.
  pub path: RelativePath,
  /// Optional media type supplied by the producing plugin.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

/// Report registration reproduced after a cache hit.
///
/// Report formats remain plugin-defined so adding a new report producer does
/// not require a new core enum or protocol version. The report file itself is
/// carried by the output bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CachedReport {
  /// User-visible report registration name.
  pub name: String,
  /// Existing report file relative to the restored workspace.
  pub path: RelativePath,
  /// Opaque plugin-defined report format.
  pub format: String,
}

/// Complete replayable result of one successful cached action.
///
/// Failed and cancelled executions are never represented by this type. It
/// contains only state required to make a hit behave like the original
/// successful task: declared filesystem outputs, bounded dependency-visible
/// stdout, public structured outputs, and resource registrations. It does not
/// preserve the full log/event stream, timing information, agent identity, or
/// any secret value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResultV1 {
  /// Result wire version; must be [`ACTION_RESULT_VERSION_V1`].
  pub result_version: u16,
  /// BLAKE3 action digest for which this result was published.
  ///
  /// Storing the key in the value prevents a result fetched from the wrong
  /// namespace or storage key from being silently accepted.
  pub action: Digest,
  /// Immutable bundle containing every declared filesystem output.
  ///
  /// It is absent only for an action whose filesystem output contract is empty.
  #[serde(
    default,
    skip_serializing_if = "Option::is_none",
    deserialize_with = "deserialize_optional_bundle"
  )]
  pub output_bundle: Option<BlobDescriptor>,
  /// Optional bounded standard output consumed as the task's logical result.
  ///
  /// This is not a transcript of process output. Octa replays it only where the
  /// existing task-dependency result semantics require the produced stdout.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stdout: Option<String>,
  /// Public structured task outputs; secret values are never cacheable.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub task_outputs: BTreeMap<String, Value>,
  /// Artifact registrations recreated after verified filesystem restoration.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub artifacts: Vec<CachedArtifact>,
  /// Plugin-defined report registrations recreated after verified filesystem restoration.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub reports: Vec<CachedReport>,
}

impl ActionResultV1 {
  /// Validates bounded, internally consistent metadata before publication or restore.
  ///
  /// Validation is deliberately symmetric: producers call it before making a
  /// record visible, and consumers call it again because local and remote cache
  /// contents are untrusted. Filesystem containment and actual blob integrity
  /// require task/output context and are consequently enforced by `octa-cache`.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    if self.result_version != ACTION_RESULT_VERSION_V1 {
      return Err(CacheProtocolError::Result(format!(
        "unsupported result version {}",
        self.result_version
      )));
    }
    if self.action.algorithm() != DigestAlgorithm::Blake3 {
      return Err(CacheProtocolError::Result("action identity must use BLAKE3".to_owned()));
    }
    if let Some(output_bundle) = &self.output_bundle {
      output_bundle.validate()?;
    } else if !self.artifacts.is_empty() || !self.reports.is_empty() {
      // Registrations point at restored paths. Without a filesystem bundle the
      // resources could be announced but would not exist after a cache hit.
      return Err(CacheProtocolError::Result(
        "artifacts and reports require an output bundle".to_owned(),
      ));
    }
    if self.artifacts.len() > MAX_CACHE_LIST_ITEMS || self.reports.len() > MAX_CACHE_LIST_ITEMS {
      return Err(CacheProtocolError::Result(format!(
        "artifact and report lists are limited to {MAX_CACHE_LIST_ITEMS} items"
      )));
    }
    if self
      .stdout
      .as_ref()
      .is_some_and(|value| value.len() > MAX_ACTION_RESULT_METADATA_BYTES)
    {
      return Err(CacheProtocolError::Result(format!(
        "stdout exceeds {MAX_ACTION_RESULT_METADATA_BYTES} UTF-8 bytes"
      )));
    }
    for (name, value) in &self.task_outputs {
      validate_result_string("task output name", name)?;
      if value.is_null() {
        return Err(CacheProtocolError::Result(format!(
          "task output '{name}' must contain a concrete public value"
        )));
      }
    }
    for artifact in &self.artifacts {
      validate_result_string("artifact name", &artifact.name)?;
      if artifact.path.is_root() {
        return Err(CacheProtocolError::Result(
          "artifact path must not be the workspace root".to_owned(),
        ));
      }
      if let Some(content_type) = &artifact.content_type {
        validate_result_string("artifact content type", content_type)?;
      }
    }
    for report in &self.reports {
      validate_result_string("report name", &report.name)?;
      validate_result_string("report format", &report.format)?;
      if report.path.is_root() {
        return Err(CacheProtocolError::Result(
          "report path must not be the workspace root".to_owned(),
        ));
      }
    }
    // Stream into a bounded counter instead of allocating an untrusted
    // serialized value merely to measure it.
    measure_json(
      &(&self.stdout, &self.task_outputs, &self.artifacts, &self.reports),
      MAX_ACTION_RESULT_METADATA_BYTES,
      "result metadata",
    )?;
    measure_json(self, MAX_ACTION_RESULT_WIRE_BYTES, "action result")?;
    Ok(())
  }
}

fn measure_json(value: &impl Serialize, maximum: usize, kind: &str) -> Result<(), CacheProtocolError> {
  let mut writer = BoundedCounter::new(maximum);
  serde_json::to_writer(&mut writer, value).map_err(|error| {
    debug_assert!(
      writer.exceeded,
      "bounded counter was the only fallible JSON sink: {error}"
    );
    CacheProtocolError::Result(format!("{kind} exceeds {maximum} bytes"))
  })
}

struct BoundedCounter {
  bytes: usize,
  maximum: usize,
  exceeded: bool,
}

impl BoundedCounter {
  fn new(maximum: usize) -> Self {
    Self {
      bytes: 0,
      maximum,
      exceeded: false,
    }
  }
}

impl Write for BoundedCounter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    let Some(next) = self.bytes.checked_add(buffer.len()) else {
      self.exceeded = true;
      return Err(io::Error::other("serialized byte count overflowed"));
    };
    if next > self.maximum {
      self.exceeded = true;
      return Err(io::Error::other("serialized value exceeds its limit"));
    }
    self.bytes = next;
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

fn deserialize_optional_bundle<'de, D>(deserializer: D) -> Result<Option<BlobDescriptor>, D::Error>
where
  D: serde::Deserializer<'de>,
{
  // The wire format distinguishes an omitted bundle from a malformed explicit
  // `null`; deserializing `Option` directly would silently accept both forms.
  BlobDescriptor::deserialize(deserializer).map(Some)
}

fn validate_result_string(kind: &str, value: &str) -> Result<(), CacheProtocolError> {
  if value.is_empty() {
    return Err(CacheProtocolError::Result(format!("{kind} must not be empty")));
  }
  if value.len() > MAX_CACHE_STRING_BYTES {
    return Err(CacheProtocolError::Result(format!(
      "{kind} exceeds {MAX_CACHE_STRING_BYTES} UTF-8 bytes"
    )));
  }
  if value.chars().any(char::is_control) {
    return Err(CacheProtocolError::Result(format!(
      "{kind} must not contain control characters"
    )));
  }
  Ok(())
}

#[cfg(test)]
mod bounded_counter_tests {
  use std::io::Write as _;

  use super::BoundedCounter;

  #[test]
  fn accepts_the_exact_limit_and_rejects_growth_and_overflow() {
    let mut counter = BoundedCounter::new(2);
    counter.write_all(b"ok").unwrap();
    counter.flush().unwrap();
    assert!(counter.write_all(b"!").is_err());
    assert!(counter.exceeded);

    // Exercise the checked-add guard independently of the ordinary maximum.
    let mut counter = BoundedCounter {
      bytes: usize::MAX,
      maximum: usize::MAX,
      exceeded: false,
    };
    assert!(counter.write(b"!").is_err());
    assert!(counter.exceeded);
  }
}
