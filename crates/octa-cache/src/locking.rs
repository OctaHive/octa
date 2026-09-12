//! Bounded names for advisory cross-process lock stripes.

/// Number of hexadecimal BLAKE3 digits used for one lock domain.
///
/// Four thousand and ninety-six stripes keep contention low without creating
/// an unbounded lock file for every cache object or workspace path ever seen.
const LOCK_SHARD_DIGITS: usize = 3;

/// Maps an arbitrary logical lock key to a stable, bounded stripe name.
pub(crate) fn shard_name(key: &[u8]) -> String {
  blake3::hash(key).to_hex()[..LOCK_SHARD_DIGITS].to_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lock_names_have_a_fixed_bounded_shape() {
    assert_eq!(shard_name(b"first").len(), LOCK_SHARD_DIGITS);
    assert_eq!(shard_name(b"first"), shard_name(b"first"));
  }
}
