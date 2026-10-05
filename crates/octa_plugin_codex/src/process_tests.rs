use super::*;

#[tokio::test]
async fn bounded_reader_accepts_the_limit_and_rejects_the_next_byte() {
  let complete = read_bounded([b'x'; 8].as_slice(), 8).await.unwrap();
  assert!(matches!(complete, CapturedStream::Complete(bytes) if bytes.len() == 8));

  let exceeded = read_bounded([b'x'; 9].as_slice(), 8).await.unwrap();
  assert!(matches!(exceeded, CapturedStream::Exceeded));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn wait_without_reaping_rejects_an_unknown_child() {
  assert!(process_exited_without_reaping(i32::MAX).is_err());
}
