//! Minimal process marker used by the dry-run contract test.
//!
//! Later process-lifecycle tests extend the fixture's behavior. For the static
//! contract, every invocation simply leaves an observable sibling marker so a
//! dry run cannot accidentally start the selected executable unnoticed.

fn main() {
  let executable = std::env::current_exe().expect("fixture executable path must be available");
  let marker = executable
    .parent()
    .expect("fixture executable must have a parent")
    .join("spawned");
  std::fs::write(marker, b"spawned").expect("fixture marker must be writable");
}
