use std::{ffi::OsString, path::Path};

use anyhow::Context;
use tempfile::{Builder, TempDir};

/// Keeps executable aliases for bundled utilities alive for the plugin lifetime.
pub(crate) struct Coreutils {
  directory: TempDir,
}

impl Coreutils {
  pub(crate) fn new() -> anyhow::Result<Self> {
    let executable = std::env::current_exe().context("Failed to locate the shell plugin executable")?;
    let directory = create_directory(&executable)?;
    let mut names = brush_coreutils_builtins::bundled_commands()
      .into_keys()
      .collect::<Vec<_>>();
    names.sort_unstable();
    create_aliases(directory.path(), &executable, &names)?;

    Ok(Self { directory })
  }

  pub(crate) fn path(&self) -> &Path {
    self.directory.path()
  }
}

#[cfg(unix)]
fn create_directory(_executable: &Path) -> anyhow::Result<TempDir> {
  Builder::new()
    .prefix("octa-coreutils-")
    .tempdir()
    .context("Failed to create the bundled coreutils directory")
}

#[cfg(windows)]
fn create_directory(executable: &Path) -> anyhow::Result<TempDir> {
  // Keeping aliases on the plugin volume lets Windows use hard links without
  // copying the executable. Read-only installations fall back to system temp.
  if let Some(parent) = executable.parent() {
    if let Ok(directory) = Builder::new().prefix("octa-coreutils-").tempdir_in(parent) {
      return Ok(directory);
    }
  }

  Builder::new()
    .prefix("octa-coreutils-")
    .tempdir()
    .context("Failed to create the bundled coreutils directory")
}

/// Runs a utility when the plugin executable was entered through one of its aliases.
pub(crate) fn dispatch() -> Option<u8> {
  let mut args = std::env::args_os();
  let invoked_as = args.next()?;
  let name = Path::new(&invoked_as).file_stem()?.to_str()?;
  let commands = brush_coreutils_builtins::bundled_commands();
  let command = commands.get(name)?;
  let command_args = std::iter::once(OsString::from(name)).chain(args).collect();

  Some(u8::try_from(command(command_args)).unwrap_or(1))
}

#[cfg(unix)]
fn create_aliases(directory: &Path, executable: &Path, names: &[String]) -> anyhow::Result<()> {
  use std::os::unix::fs::symlink;

  for name in names {
    symlink(executable, directory.join(name)).with_context(|| format!("Failed to create bundled command '{name}'"))?;
  }

  Ok(())
}

#[cfg(windows)]
fn create_aliases(directory: &Path, executable: &Path, names: &[String]) -> anyhow::Result<()> {
  use std::fs;

  let Some((first, remaining)) = names.split_first() else {
    return Ok(());
  };
  let first_path = directory.join(format!("{first}.exe"));
  let link_source = match fs::hard_link(executable, &first_path) {
    Ok(()) => executable.to_path_buf(),
    Err(_) => {
      // Temporary storage can be on another volume than the installed plugin.
      // Copy once, then hard-link all command names to that local executable.
      let local_executable = directory.join("octa-coreutils.exe");
      fs::copy(executable, &local_executable).context("Failed to copy the bundled coreutils executable")?;
      fs::hard_link(&local_executable, &first_path)
        .with_context(|| format!("Failed to create bundled command '{first}'"))?;
      local_executable
    },
  };

  for name in remaining {
    fs::hard_link(&link_source, directory.join(format!("{name}.exe")))
      .with_context(|| format!("Failed to create bundled command '{name}'"))?;
  }

  Ok(())
}
