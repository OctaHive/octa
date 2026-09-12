//! Version-one task action identity and its canonical binary encoding.
//!
//! An action key answers one question: “may the successful result of this task
//! invocation be reused instead of executing it?” Two invocations receive the
//! same key only when every declared input capable of changing the result is
//! identical. The action cache maps that key to an [`crate::ActionResultV1`]; it
//! is not a task name, a workspace-local freshness slot, or a
//! storage-implementation-specific object name.
//!
//! Conceptually, the key is:
//!
//! ```text
//! BLAKE3(
//!   CanonicalEncode(
//!     key format,
//!     resolved task definition,
//!     input filesystem tree,
//!     relative working directory,
//!     public variables and environment,
//!     runtime identity,
//!     participating plugin identities,
//!     ordered arguments,
//!     timeout,
//!     user salt,
//!   )
//! )
//! ```
//!
//! Each category closes a different false-hit path:
//!
//! - `task_definition` changes when executable task semantics or the declared
//!   file contract changes;
//! - `input_root` identifies paths, entry kinds, executable bits, link targets,
//!   and file contents without depending on timestamps or an absolute root;
//! - variables, environment, arguments, working directory, and timeout capture
//!   resolved invocation semantics that may not appear verbatim in the task;
//! - `runtime` prevents results crossing incompatible host toolchains or OCI
//!   images;
//! - plugin identities prevent a result produced by one implementation from
//!   being reused after that implementation or its protocol changes;
//! - `salt` provides an explicit escape hatch when an external semantic input
//!   cannot otherwise be represented.
//!
//! Absolute workspace paths, agent/job identifiers, file timestamps, cache
//! endpoints, credentials, read/write policy, and presentation settings are
//! deliberately absent. Excluding them makes equivalent work portable between
//! workspaces and agents. The executor must reject secret-bearing tasks as
//! uncacheable; callers constructing this descriptor must hash only the public
//! variable and environment values selected by that policy.
//!
//! ## Why a custom canonical encoding
//!
//! JSON is retained as the bounded wire and diagnostic representation, but it
//! is not hashed: object ordering, optional-field spelling, and serializer
//! behavior are not a suitable long-lived cache-key contract. The encoder below
//! writes an allocation-free, domain-separated stream with fixed field tags,
//! explicit collection counts, length-prefixed strings, variant discriminants,
//! and big-endian integers. These boundaries prevent ambiguous concatenations.
//! Plugin order is normalized because plugins form a uniquely named set;
//! argument order is preserved because it is semantic. Optional values encode
//! an explicit presence byte, so absence cannot collide with a zero or empty
//! value.
//! `key_format` is both validated and hashed so the wire value self-identifies;
//! the separate domain prevents these bytes from colliding with another Octa
//! object format that happens to contain the same fields.
//!
//! Every numeric tag and byte layout in this module is part of
//! `octa.action.v1`. Adding a key-relevant field, changing normalization, or
//! changing an encoding requires a new key-format version and domain rather
//! than silently invalidating or, worse, aliasing existing cache records.

use std::{collections::BTreeSet, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
  validate_string, CacheProtocolError, Digest, DigestAlgorithm, RelativePath, MAX_CACHE_LIST_ITEMS,
  MAX_CACHE_STRING_BYTES,
};

/// Canonical action-key format understood by this crate version.
pub const ACTION_KEY_FORMAT_V1: u16 = 1;
/// Separates action descriptors from every other BLAKE3-hashed Octa format.
const ACTION_DOMAIN: &[u8] = b"octa.action.v1";

/// Operating system that constrains a cached action result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformOs {
  /// Linux userspace on a Linux-compatible kernel.
  Linux,
  /// Microsoft Windows userspace on a Windows kernel.
  Windows,
  /// Apple macOS userspace on a Darwin kernel.
  Macos,
}

impl PlatformOs {
  /// Returns the stable byte assigned by the version-one action format.
  fn tag(self) -> u8 {
    match self {
      Self::Linux => 1,
      Self::Windows => 2,
      Self::Macos => 3,
    }
  }
}

/// CPU architecture that constrains a cached action result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformArchitecture {
  /// 64-bit x86 architecture, also known as x86_64.
  Amd64,
  /// 64-bit Arm architecture, also known as aarch64.
  Arm64,
}

impl PlatformArchitecture {
  /// Returns the stable byte assigned by the version-one action format.
  fn tag(self) -> u8 {
    match self {
      Self::Amd64 => 1,
      Self::Arm64 => 2,
    }
  }
}

/// Immutable execution environment included in an action identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeIdentity {
  /// Direct host execution, safe to share only with an explicit environment digest.
  Native {
    /// Host operating system on which the result was produced.
    os: PlatformOs,
    /// Host CPU architecture on which the result was produced.
    architecture: PlatformArchitecture,
    /// BLAKE3 identity of the compiler, tools, and relevant host configuration.
    environment: Digest,
  },
  /// OCI execution identified by an immutable guest image digest.
  Oci {
    /// Guest operating system described by the image manifest.
    os: PlatformOs,
    /// Guest CPU architecture described by the image manifest.
    architecture: PlatformArchitecture,
    /// SHA-256 digest of the immutable OCI image manifest.
    image: Digest,
  },
}

impl RuntimeIdentity {
  /// Validates the immutable environment identity accepted by Octa execution.
  ///
  /// Native environments are Octa-owned BLAKE3 identities. OCI images retain
  /// the registry's SHA-256 manifest identity, and macOS is not an OCI guest
  /// supported by Octa's current execution model.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    match self {
      Self::Native { environment, .. } => {
        require_algorithm("Native runtime environment", *environment, DigestAlgorithm::Blake3)
      },
      Self::Oci {
        os: PlatformOs::Macos, ..
      } => Err(CacheProtocolError::Configuration(
        "OCI cache runtime cannot declare a macOS guest".to_owned(),
      )),
      Self::Oci { image, .. } => require_algorithm("OCI image", *image, DigestAlgorithm::Sha256),
    }
  }
}

/// Exact identity of one plugin that contributes to task semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginIdentity {
  /// Stable plugin name used by the task definition.
  pub name: String,
  /// Plugin implementation version reported by its manifest.
  pub version: String,
  /// Version of the Octa plugin protocol spoken by the executable.
  pub protocol_version: u16,
  /// SHA-256 digest of the exact plugin executable.
  pub executable: Digest,
}

/// Every semantic input used to derive a portable action digest.
///
/// The descriptor is intentionally a plain validated wire type, not an
/// executor-aware builder. The executor owns resolution of task configuration,
/// public values, runtime, and participating plugins; this crate owns only the
/// portable shape, bounds, and canonical digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDescriptorV1 {
  /// Canonical action-key encoding version; must be [`ACTION_KEY_FORMAT_V1`].
  pub key_format: u16,
  /// BLAKE3 identity of the resolved semantic task definition.
  pub task_definition: Digest,
  /// BLAKE3 identity of the canonical input filesystem tree.
  pub input_root: Digest,
  /// Task working directory relative to the workspace.
  pub working_directory: RelativePath,
  /// BLAKE3 identity of resolved public task variables.
  pub variables: Digest,
  /// BLAKE3 identity of configured and explicitly inherited task environment values.
  pub environment: Digest,
  /// Execution platform and immutable runtime environment identity.
  pub runtime: RuntimeIdentity,
  /// Exact plugins that contribute to the task semantics.
  pub plugins: Vec<PluginIdentity>,
  /// User-supplied command arguments in semantic order.
  pub arguments: Vec<String>,
  /// Effective task timeout; absence and a concrete duration are distinct identities.
  #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_duration")]
  pub timeout: Option<Duration>,
  /// Optional user-controlled invalidation value.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub salt: Option<String>,
}

impl ActionDescriptorV1 {
  /// Validates version-one bounds and identity invariants.
  ///
  /// Validation rejects ambiguous set members, unsupported digest algorithms,
  /// and unbounded wire values. It cannot prove that the caller supplied the
  /// correct task, input, or environment digest; that belongs to the executor
  /// that constructs the descriptor.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    if self.key_format != ACTION_KEY_FORMAT_V1 {
      return Err(CacheProtocolError::Action(format!(
        "unsupported key format {}",
        self.key_format
      )));
    }
    if self.plugins.len() > MAX_CACHE_LIST_ITEMS || self.arguments.len() > MAX_CACHE_LIST_ITEMS {
      return Err(CacheProtocolError::Action(format!(
        "plugin and argument lists are limited to {MAX_CACHE_LIST_ITEMS} items"
      )));
    }
    require_algorithm("task definition", self.task_definition, DigestAlgorithm::Blake3)?;
    require_algorithm("input root", self.input_root, DigestAlgorithm::Blake3)?;
    require_algorithm("variables", self.variables, DigestAlgorithm::Blake3)?;
    require_algorithm("environment", self.environment, DigestAlgorithm::Blake3)?;
    self.runtime.validate()?;
    let mut plugin_names = BTreeSet::new();
    for plugin in &self.plugins {
      validate_string("plugin name", &plugin.name)?;
      validate_string("plugin version", &plugin.version)?;
      if plugin.protocol_version == 0 {
        return Err(CacheProtocolError::Action(format!(
          "plugin '{}' has protocol version zero",
          plugin.name
        )));
      }
      if plugin.executable.algorithm() != DigestAlgorithm::Sha256 {
        return Err(CacheProtocolError::Action(format!(
          "plugin '{}' executable must retain its SHA-256 identity",
          plugin.name
        )));
      }
      // Plugins are canonicalized by name during encoding. Duplicate names
      // would make that set ambiguous even if their remaining fields differed.
      if !plugin_names.insert(&plugin.name) {
        return Err(CacheProtocolError::Action(format!(
          "plugin '{}' appears more than once",
          plugin.name
        )));
      }
    }
    for argument in &self.arguments {
      if argument.len() > MAX_CACHE_STRING_BYTES {
        return Err(CacheProtocolError::Action(format!(
          "argument exceeds {MAX_CACHE_STRING_BYTES} UTF-8 bytes"
        )));
      }
    }
    if self.timeout.is_some_and(|timeout| timeout.is_zero()) {
      return Err(CacheProtocolError::Action(
        "timeout must be greater than zero".to_owned(),
      ));
    }
    if let Some(salt) = &self.salt {
      validate_string("salt", salt)?;
    }
    Ok(())
  }

  /// Computes the stable `octa.action.v1` digest.
  ///
  /// Field numbers and encodings below are the persisted version-one format,
  /// not implementation details that may be reordered during refactoring.
  /// Plugin order is normalized because the identity is a set. Argument order
  /// is retained because it can change command semantics.
  pub fn digest(&self) -> Result<Digest, CacheProtocolError> {
    self.validate()?;
    let mut encoder = CanonicalEncoder::new(ACTION_DOMAIN);
    encoder.u16(1, self.key_format);
    encoder.digest(2, self.task_definition);
    encoder.digest(3, self.input_root);
    encoder.string(4, self.working_directory.as_str());
    encoder.digest(5, self.variables);
    encoder.digest(6, self.environment);
    encoder.runtime(7, &self.runtime);

    // Validation made names unique, so sorting only by name gives every producer
    // exactly one canonical order without considering registration order.
    let mut plugins = self.plugins.iter().collect::<Vec<_>>();
    plugins.sort_by(|left, right| left.name.cmp(&right.name));
    encoder.u32(8, plugins.len() as u32);
    for plugin in plugins {
      encoder.string(9, &plugin.name);
      encoder.string(10, &plugin.version);
      encoder.u16(11, plugin.protocol_version);
      encoder.digest(12, plugin.executable);
    }

    encoder.u32(13, self.arguments.len() as u32);
    for argument in &self.arguments {
      encoder.string(14, argument);
    }
    // Presence markers distinguish `None` from every concrete value. In
    // particular, absence must not alias a present zero duration or empty salt.
    match self.timeout {
      Some(timeout) => {
        encoder.byte(15, 1);
        encoder.u64(16, timeout.as_secs());
        encoder.u32(17, timeout.subsec_nanos());
      },
      None => encoder.byte(15, 0),
    }
    match &self.salt {
      Some(salt) => {
        encoder.byte(18, 1);
        encoder.string(19, salt);
      },
      None => encoder.byte(18, 0),
    }
    Ok(encoder.finish())
  }
}

fn require_algorithm(kind: &str, digest: Digest, expected: DigestAlgorithm) -> Result<(), CacheProtocolError> {
  if digest.algorithm() != expected {
    return Err(CacheProtocolError::Action(format!(
      "{kind} must use the {expected} digest algorithm"
    )));
  }
  Ok(())
}

/// Streams one canonical descriptor directly into BLAKE3.
///
/// `bytes` becomes [`Digest::size_bytes`](crate::Digest::size_bytes) and counts
/// the encoded descriptor bytes, not input-file bytes. Retaining it lets stores
/// and diagnostics describe what was hashed without buffering the encoding.
struct CanonicalEncoder {
  hasher: blake3::Hasher,
  bytes: u64,
}

impl CanonicalEncoder {
  fn new(domain: &[u8]) -> Self {
    let mut result = Self {
      hasher: blake3::Hasher::new(),
      bytes: 0,
    };
    // Length-prefix the domain as well: domain separation must not depend on a
    // sentinel byte that a future format could accidentally embed.
    result.raw(&(domain.len() as u32).to_be_bytes());
    result.raw(domain);
    result
  }

  fn raw(&mut self, value: &[u8]) {
    self.hasher.update(value);
    self.bytes += value.len() as u64;
  }

  fn tag(&mut self, tag: u8) {
    // Tags separate adjacent fields; strings and collections add their own
    // lengths/counts so no pair of logical values shares an encoding.
    self.raw(&[tag]);
  }

  fn byte(&mut self, tag: u8, value: u8) {
    self.tag(tag);
    self.raw(&[value]);
  }

  fn u16(&mut self, tag: u8, value: u16) {
    self.tag(tag);
    self.raw(&value.to_be_bytes());
  }

  fn u32(&mut self, tag: u8, value: u32) {
    self.tag(tag);
    self.raw(&value.to_be_bytes());
  }

  fn u64(&mut self, tag: u8, value: u64) {
    self.tag(tag);
    self.raw(&value.to_be_bytes());
  }

  fn string(&mut self, tag: u8, value: &str) {
    self.tag(tag);
    self.raw(&(value.len() as u32).to_be_bytes());
    self.raw(value.as_bytes());
  }

  fn digest(&mut self, tag: u8, value: Digest) {
    self.tag(tag);
    // Preserve the source algorithm and size. External SHA-256 identities must
    // not be reinterpreted as native BLAKE3 values with the same 32 bytes.
    self.raw(&[value.algorithm().canonical_tag()]);
    self.raw(&value.size_bytes().to_be_bytes());
    self.raw(&value.bytes());
  }

  fn runtime(&mut self, tag: u8, runtime: &RuntimeIdentity) {
    self.tag(tag);
    // The leading variant byte prevents a Native environment digest from ever
    // aliasing an OCI image digest with otherwise equal platform bytes.
    match runtime {
      RuntimeIdentity::Native {
        os,
        architecture,
        environment,
      } => {
        self.raw(&[1, os.tag(), architecture.tag()]);
        self.digest(20, *environment);
      },
      RuntimeIdentity::Oci {
        os,
        architecture,
        image,
      } => {
        self.raw(&[2, os.tag(), architecture.tag()]);
        self.digest(21, *image);
      },
    }
  }

  fn finish(self) -> Digest {
    Digest::new(DigestAlgorithm::Blake3, *self.hasher.finalize().as_bytes(), self.bytes)
  }
}

mod optional_duration {
  use std::time::Duration;

  use serde::{Deserialize, Deserializer, Serialize, Serializer};

  #[derive(Deserialize, Serialize)]
  #[serde(deny_unknown_fields)]
  struct WireDuration {
    seconds: u64,
    nanoseconds: u32,
  }

  pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    value
      .map(|duration| WireDuration {
        seconds: duration.as_secs(),
        nanoseconds: duration.subsec_nanos(),
      })
      .serialize(serializer)
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = Option::<WireDuration>::deserialize(deserializer)?;
    value
      .map(|duration| {
        if duration.nanoseconds >= 1_000_000_000 {
          return Err(serde::de::Error::custom("nanoseconds must be less than one billion"));
        }
        Ok(Duration::new(duration.seconds, duration.nanoseconds))
      })
      .transpose()
  }
}
