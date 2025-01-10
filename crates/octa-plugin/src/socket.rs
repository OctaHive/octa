use std::ffi::{OsStr, OsString};

/// Generate a name to be used for a local socket specific to this `octa` process, described by the
/// given `unique_id`, which should be unique to the purpose of the socket.
///
/// On Unix, this is a path, which should generally be 100 characters or less for compatibility. On
/// Windows, this is a name within the `\\.\pipe` namespace.
#[cfg(unix)]
pub fn make_local_socket_name(unique_id: &str) -> OsString {
  // Prefer to put it in OCTA_RUNTIME_DIR if set, since that's user-local
  let mut base = if let Some(runtime_dir) = std::env::var_os("OCTA_RUNTIME_DIR") {
    std::path::PathBuf::from(runtime_dir)
  } else {
    // Use std::env::temp_dir() for portability, especially since on Android this is probably
    // not `/tmp`
    std::env::temp_dir()
  };
  let socket_name = format!("octa.{}.{}.sock", std::process::id(), unique_id);
  base.push(socket_name);
  base.into()
}

/// Interpret a local socket name for use with `interprocess`.
#[cfg(unix)]
pub fn interpret_local_socket_name(name: &OsStr) -> Result<interprocess::local_socket::Name, std::io::Error> {
  use interprocess::local_socket::{GenericFilePath, ToFsName};

  name.to_fs_name::<GenericFilePath>()
}

/// Generate a name to be used for a local socket specific to this `octa` process, described by the
/// given `unique_id`, which should be unique to the purpose of the socket.
///
/// On Unix, this is a path, which should generally be 100 characters or less for compatibility. On
/// Windows, this is a name within the `\\.\pipe` namespace.
#[cfg(windows)]
pub fn make_local_socket_name(unique_id: &str) -> OsString {
  format!("octa.{}.{}", std::process::id(), unique_id).into()
}

/// Interpret a local socket name for use with `interprocess`.
#[cfg(windows)]
pub fn interpret_local_socket_name(name: &OsStr) -> Result<interprocess::local_socket::Name, std::io::Error> {
  use interprocess::local_socket::{GenericNamespaced, ToNsName};

  name.to_ns_name::<GenericNamespaced>()
}
