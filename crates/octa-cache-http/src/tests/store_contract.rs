//! Shared immutable-store contract exercised by local and HTTP adapters.

use super::*;

/// Exercises only the public storage boundary so both implementations retain
/// identical immutable-object and namespace semantics as their internals evolve.
async fn cache_store_contract(store: Arc<dyn CacheStore>, expected_layer: CacheLayer) {
  let (blob, bytes, result) = fixture();
  let namespace = "contract/primary";
  assert!(store.get_action(namespace, &result.action).await.unwrap().is_none());
  assert!(store.read_blob(&blob).await.is_err());
  assert_eq!(
    store.find_missing_blobs(std::slice::from_ref(&blob)).await.unwrap(),
    std::slice::from_ref(&blob)
  );

  assert_eq!(
    store
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
      .await
      .unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    store
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
      .await
      .unwrap(),
    WriteOutcome::AlreadyPresent
  );
  assert!(store
    .find_missing_blobs(std::slice::from_ref(&blob))
    .await
    .unwrap()
    .is_empty());
  let mut restored = Vec::new();
  store
    .read_blob(&blob)
    .await
    .unwrap()
    .read_to_end(&mut restored)
    .await
    .unwrap();
  assert_eq!(restored, bytes);

  assert_eq!(
    store.write_action_if_absent(namespace, &result).await.unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    store.write_action_if_absent(namespace, &result).await.unwrap(),
    WriteOutcome::AlreadyPresent
  );
  let lookup = store.get_action(namespace, &result.action).await.unwrap().unwrap();
  assert_eq!(lookup.result, result);
  assert_eq!(lookup.layer, expected_layer);
  assert!(store
    .get_action("contract/isolated", &result.action)
    .await
    .unwrap()
    .is_none());

  let mut conflicting = result;
  conflicting.stdout = Some("different result".to_owned());
  assert_eq!(
    store.write_action_if_absent(namespace, &conflicting).await.unwrap(),
    WriteOutcome::Conflict
  );

  let concurrent_namespace = "contract/concurrent";
  let mut equal = conflicting.clone();
  equal.action = Digest::blake3(b"concurrent-equal-action");
  equal.stdout = Some("same result".to_owned());
  let (left, right) = tokio::join!(
    store.write_action_if_absent(concurrent_namespace, &equal),
    store.write_action_if_absent(concurrent_namespace, &equal),
  );
  let equal_outcomes = [left.unwrap(), right.unwrap()];
  assert_eq!(
    equal_outcomes
      .iter()
      .filter(|outcome| **outcome == WriteOutcome::Written)
      .count(),
    1
  );
  assert_eq!(
    equal_outcomes
      .iter()
      .filter(|outcome| **outcome == WriteOutcome::AlreadyPresent)
      .count(),
    1
  );

  let conflict_action = Digest::blake3(b"concurrent-conflicting-action");
  let mut first = equal.clone();
  first.action = conflict_action;
  first.stdout = Some("first result".to_owned());
  let mut second = first.clone();
  second.stdout = Some("second result".to_owned());
  let (left, right) = tokio::join!(
    store.write_action_if_absent(concurrent_namespace, &first),
    store.write_action_if_absent(concurrent_namespace, &second),
  );
  let conflict_outcomes = [left.unwrap(), right.unwrap()];
  assert_eq!(
    conflict_outcomes
      .iter()
      .filter(|outcome| **outcome == WriteOutcome::Written)
      .count(),
    1
  );
  assert_eq!(
    conflict_outcomes
      .iter()
      .filter(|outcome| **outcome == WriteOutcome::Conflict)
      .count(),
    1
  );
}

#[tokio::test]
async fn local_store_satisfies_the_shared_cache_contract() {
  let directory = TempDir::new().unwrap();
  let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(directory.path())).unwrap());
  cache_store_contract(store, CacheLayer::Local).await;
}

#[tokio::test]
async fn http_store_satisfies_the_shared_cache_contract() {
  let server = ReferenceServer::start().await;
  let store = Arc::new(client(&server));
  assert!(store.recover_corrupt_blob(&fixture().0).await.unwrap().is_none());
  cache_store_contract(store, CacheLayer::Remote).await;
}
