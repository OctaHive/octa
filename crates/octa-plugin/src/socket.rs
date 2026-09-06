use std::ffi::{OsStr, OsString};

/// Generate a name to be used for a local socket specific to this `octa` process, described by the
/// given `unique_id`, which should be unique to the purpose of the socket.
///
/// On Unix, this is a path, which should generally be 100 characters or less for compatibility. On
/// Windows, this is a name within the `\\.\pipe` namespace.
#[cfg(unix)]
pub fn make_local_socket_name(unique_id: &str) -> OsString {
  let runtime_dir = std::env::var_os("OCTA_RUNTIME_DIR");
  make_unix_socket_name(unique_id, runtime_dir.as_deref())
}

#[cfg(unix)]
fn make_unix_socket_name(unique_id: &str, runtime_dir: Option<&OsStr>) -> OsString {
  // Prefer OCTA_RUNTIME_DIR when supplied. temp_dir() is the portable fallback, especially on
  // platforms such as Android where the temporary directory is not necessarily `/tmp`.
  let mut base = runtime_dir
    .map(std::path::PathBuf::from)
    .unwrap_or_else(std::env::temp_dir);
  let socket_name = format!("octa.{}.{}.sock", std::process::id(), unique_id);
  base.push(socket_name);
  base.into()
}

/// Interpret a local socket name for use with `interprocess`.
#[cfg(unix)]
pub fn interpret_local_socket_name(name: &OsStr) -> Result<interprocess::local_socket::Name<'_>, std::io::Error> {
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
pub fn interpret_local_socket_name(name: &OsStr) -> Result<interprocess::local_socket::Name<'_>, std::io::Error> {
  use interprocess::local_socket::{GenericNamespaced, ToNsName};

  name.to_ns_name::<GenericNamespaced>()
}

#[cfg(test)]
mod tests {
  use super::*;
  #[cfg(unix)]
  use std::path::PathBuf;

  #[test]
  fn test_make_local_socket_name_format() {
    let unique_id = "test123";
    let socket_name = make_local_socket_name(unique_id);
    let pid = std::process::id();

    #[cfg(unix)]
    {
      let path = PathBuf::from(&socket_name);
      let file_name = path.file_name().unwrap().to_str().unwrap();
      assert!(file_name.starts_with("octa."));
      assert!(file_name.ends_with(".sock"));
      assert!(file_name.contains(&pid.to_string()));
      assert!(file_name.contains(unique_id));
    }

    #[cfg(windows)]
    {
      let name = socket_name.to_str().unwrap();
      assert!(name.starts_with("octa."));
      assert!(name.contains(&pid.to_string()));
      assert!(name.contains(unique_id));
    }
  }

  #[test]
  fn test_make_local_socket_name_uniqueness() {
    let name1 = make_local_socket_name("test1");
    let name2 = make_local_socket_name("test2");
    assert_ne!(name1, name2);
  }

  #[test]
  fn test_interpret_local_socket_name() {
    let socket_name = make_local_socket_name("test123");
    let result = interpret_local_socket_name(&socket_name);
    assert!(result.is_ok());
  }

  #[cfg(unix)]
  #[test]
  fn test_unix_socket_path_with_runtime_dir() {
    let test_dir = "/test/runtime/dir";
    let socket_name = make_unix_socket_name("test123", Some(OsStr::new(test_dir)));
    let path = PathBuf::from(&socket_name);

    assert!(path.starts_with(test_dir));
  }

  #[cfg(unix)]
  #[test]
  fn test_unix_socket_path_with_temp_dir() {
    let socket_name = make_unix_socket_name("test123", None);
    let path = PathBuf::from(&socket_name);

    assert!(path.starts_with(std::env::temp_dir()));
  }

  #[test]
  fn test_invalid_socket_name_interpretation() {
    // Test with an invalid socket name
    let invalid_name = OsString::from("invalid\0name");
    let result = interpret_local_socket_name(&invalid_name);
    assert!(result.is_err());
  }

  #[cfg(windows)]
  #[test]
  fn test_windows_pipe_name_format() {
    let socket_name = make_local_socket_name("test123");
    let name = socket_name.to_str().unwrap();

    // Windows named pipes should follow specific naming conventions
    assert!(!name.contains('\\'));
    assert!(!name.contains('/'));
  }
}
