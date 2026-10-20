//! lwjgl3ify support for Minecraft 1.7.10 + Forge.
//!
//! lwjgl3ify runs the game on LWJGL 3 / modern Java by relaunching itself with
//! RetroFuturaBootstrap. Following Prism Launcher's approach, the launcher does
//! not reproduce that relaunch: it only guarantees the first JVM is modern
//! enough for the relaunched game, then lets the mod do the rest.
//!
//! Nothing here may depend on lwjgl3ify's internal files (`version.json`,
//! `forgePatches.zip`, ...): they are private implementation details of the mod
//! and change between versions without notice.

use std::path::Path;

use crate::instance::models::misc::{Instance, ModLoaderType};

/// Minimum Java major version required by lwjgl3ify (RetroFuturaBootstrap).
pub const MIN_JAVA_VERSION: i32 = 17;

/// 1.7.10 + Forge is the only combination lwjgl3ify supports.
pub fn is_supported_instance(instance: &Instance) -> bool {
  instance.version == "1.7.10"
    && matches!(
      instance.mod_loader.loader_type,
      ModLoaderType::Forge | ModLoaderType::LegacyForge
    )
}

/// Whether an enabled `lwjgl3ify` jar is present in `mods_dir`.
///
/// This inspects file names only, so it costs one directory listing and never
/// fails on a corrupt archive. Forge also scans the `1.7.10` subdirectory of
/// `mods`, so both locations are checked.
pub fn is_enabled(mods_dir: &Path) -> bool {
  let versioned_dir = mods_dir.join("1.7.10");
  std::iter::once(mods_dir)
    .chain(std::iter::once(versioned_dir.as_path()))
    .any(dir_contains_lwjgl3ify)
}

fn dir_contains_lwjgl3ify(dir: &Path) -> bool {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return false;
  };

  entries.flatten().any(|entry| {
    let file_name = entry.file_name();
    let Some(file_name) = file_name.to_str() else {
      return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    file_name.ends_with(".jar") && file_name.contains("lwjgl3ify")
  })
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  fn temp_mods_dir(suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
      "sjmcl-lwjgl3ify-{}-{}",
      suffix,
      std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  #[test]
  fn detects_enabled_jar_and_ignores_disabled_one() {
    let dir = temp_mods_dir("basic");

    assert!(!is_enabled(&dir));

    fs::write(dir.join("lwjgl3ify-3.0.37.jar.disabled"), b"").unwrap();
    assert!(!is_enabled(&dir));

    fs::write(dir.join("lwjgl3ify-3.0.37.jar"), b"").unwrap();
    assert!(is_enabled(&dir));

    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn detects_jar_inside_versioned_mods_dir() {
    let dir = temp_mods_dir("versioned");
    fs::create_dir_all(dir.join("1.7.10")).unwrap();
    fs::write(dir.join("1.7.10").join("LWJGL3ify-3.0.37.jar"), b"").unwrap();

    assert!(is_enabled(&dir));

    fs::remove_dir_all(&dir).unwrap();
  }
}
