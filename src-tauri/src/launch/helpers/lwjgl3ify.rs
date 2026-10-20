use sha1::{Digest, Sha1};
use sjmcl_types::error::{SJMCLError, SJMCLResult};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use zip::ZipArchive;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

use crate::instance::helpers::client_json::{
  ArgumentsItem, DownloadsArtifact, JavaVersion, LibrariesValue, McClientInfo,
};
use crate::instance::helpers::misc::get_instance_subdir_paths;
use crate::instance::helpers::mods::common::discover_local_mod_paths;
use crate::instance::helpers::mods::legacy_forge::read_legacy_forge_mod_metadata;
use crate::instance::models::misc::{Instance, InstanceError, InstanceSubdirType, ModLoaderType};
use crate::launch::helpers::file_validator::{convert_library_name_to_path, merge_library_lists};
use crate::launch::models::LaunchError;
use crate::launcher_config::models::{GameConfig, GameJava, JavaInfo};
use crate::utils::fs::validate_sha1;

const RESOURCE_ROOT: &str = "me/eigenraven/lwjgl3ify/relauncher/";
const SUPPORTED_VERSION: &str = "3.0.37";
const RFB_MAIN: &str = "com.gtnewhorizons.retrofuturabootstrap.MainStartOnFirstThread";
const RFB_LOADER: &str =
  "-Djava.system.class.loader=com.gtnewhorizons.retrofuturabootstrap.RfbSystemClassLoader";
const JAVA_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

// Only runtime preparation data: never persisted into the instance's version JSON.
#[derive(Debug, Clone)]
pub struct Lwjgl3ifyLaunchInfo {
  mod_path: PathBuf,
  patch: DownloadsArtifact,
}

fn compatibility_error(error: impl Into<SJMCLError>, reason: impl std::fmt::Display) -> SJMCLError {
  log::error!("lwjgl3ify: {reason}");
  error.into()
}

pub async fn resolve_lwjgl3ify(
  app: &AppHandle,
  instance: &Instance,
  game_config: &GameConfig,
) -> SJMCLResult<Option<(Lwjgl3ifyLaunchInfo, McClientInfo)>> {
  if instance.version != "1.7.10"
    || !matches!(
      instance.mod_loader.loader_type,
      ModLoaderType::Forge | ModLoaderType::LegacyForge
    )
  {
    return Ok(None);
  }
  let dirs = get_instance_subdir_paths(app, instance, &[&InstanceSubdirType::Mods])
    .ok_or(InstanceError::InstanceNotFoundByID)?;
  let mods_dir = dirs[0].clone();
  let loader_type = instance.mod_loader.loader_type;
  let detected =
    tokio::task::spawn_blocking(move || find_lwjgl3ify(&mods_dir, loader_type)).await??;
  let Some((info, profile)) = detected else {
    return Ok(None);
  };
  validate_custom_jvm_arguments(&game_config.advanced.jvm.args)?;
  Ok(Some((info, profile)))
}

pub async fn select_lwjgl3ify_java(
  app: &AppHandle,
  game_java: &GameJava,
  minimum: i32,
) -> SJMCLResult<(JavaInfo, String)> {
  let mut candidates = app.state::<Mutex<Vec<JavaInfo>>>().lock()?.clone();
  if !game_java.auto {
    let java = candidates
      .into_iter()
      .find(|java| java.exec_path == game_java.exec_path)
      .ok_or(LaunchError::SelectedJavaUnavailable)?;
    let exec_path = java.exec_path.clone();
    let platform =
      tokio::task::spawn_blocking(move || inspect_java_runtime(&exec_path, minimum)).await??;
    return Ok((java, platform));
  }

  candidates.retain(|java| (minimum.max(17)..=26).contains(&java.major_version));
  candidates.sort_by_key(|java| java.major_version);
  for java in candidates {
    let exec_path = java.exec_path.clone();
    match tokio::task::spawn_blocking(move || inspect_java_runtime(&exec_path, minimum)).await? {
      Ok(platform) => return Ok((java, platform)),
      Err(error) => log::warn!(
        "Skipping Java {} for lwjgl3ify: {}",
        java.exec_path,
        error.0
      ),
    }
  }
  Err(LaunchError::NoSuitableJava.into())
}

fn inspect_java_runtime(exec_path: &str, minimum: i32) -> SJMCLResult<String> {
  let mut command = Command::new(exec_path);
  command.args(["-XshowSettings:properties", "-version"]);
  #[cfg(target_os = "windows")]
  command.creation_flags(0x08000000); // CREATE_NO_WINDOW
  let properties = java_probe_output(&mut command, JAVA_PROBE_TIMEOUT)?;
  java_platform_from_properties(&properties, std::env::consts::OS, minimum)
}

fn java_probe_output(command: &mut Command, timeout: Duration) -> SJMCLResult<String> {
  let mut child = command
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|_| LaunchError::SelectedJavaUnavailable)?;
  let stdout = child.stdout.take().expect("stdout is piped");
  let stderr = child.stderr.take().expect("stderr is piped");
  thread::scope(|scope| {
    // Drain both pipes while waiting, so a full pipe cannot stall the Java process.
    let stdout = scope.spawn(move || read_probe_pipe(stdout));
    let stderr = scope.spawn(move || read_probe_pipe(stderr));
    let started = Instant::now();
    let status = loop {
      match child.try_wait() {
        Ok(Some(status)) => break status,
        Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
        Ok(None) => {
          log::warn!(
            "Java runtime probe timed out after {} seconds",
            timeout.as_secs_f32()
          );
          let _ = child.kill();
          let _ = child.wait();
          return Err(LaunchError::SelectedJavaUnavailable.into());
        }
        Err(error) => {
          let _ = child.kill();
          let _ = child.wait();
          return Err(error.into());
        }
      }
    };
    let stdout = stdout
      .join()
      .map_err(|_| LaunchError::SelectedJavaUnavailable)??;
    let stderr = stderr
      .join()
      .map_err(|_| LaunchError::SelectedJavaUnavailable)??;
    if !status.success() {
      return Err(LaunchError::SelectedJavaUnavailable.into());
    }
    Ok(format!(
      "{}\n{}",
      String::from_utf8_lossy(&stdout),
      String::from_utf8_lossy(&stderr)
    ))
  })
}

fn read_probe_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
  let mut bytes = Vec::new();
  pipe.read_to_end(&mut bytes)?;
  Ok(bytes)
}

fn java_platform_from_properties(properties: &str, os: &str, minimum: i32) -> SJMCLResult<String> {
  let property = |name: &str| {
    properties.lines().find_map(|line| {
      let (key, value) = line.split_once('=')?;
      (key.trim() == name).then_some(value.trim())
    })
  };
  let version = property("java.version").ok_or(LaunchError::SelectedJavaUnavailable)?;
  validate_lwjgl3ify_java(version, minimum)?;
  let arch = property("os.arch").ok_or(LaunchError::SelectedJavaUnavailable)?;
  platform_name(os, arch)
}

fn read_entry<R: Read + Seek>(
  jar: &mut ZipArchive<R>,
  name: &str,
  limit: u64,
) -> SJMCLResult<Vec<u8>> {
  let mut entry = jar.by_name(name)?;
  if entry.size() > limit {
    return Err(SJMCLError(format!("Oversized lwjgl3ify resource: {name}")));
  }
  let mut contents = Vec::new();
  entry.by_ref().take(limit + 1).read_to_end(&mut contents)?;
  if contents.len() as u64 > limit {
    return Err(SJMCLError(format!("Oversized lwjgl3ify resource: {name}")));
  }
  Ok(contents)
}

fn find_lwjgl3ify(
  mods_dir: &Path,
  loader_type: ModLoaderType,
) -> SJMCLResult<Option<(Lwjgl3ifyLaunchInfo, McClientInfo)>> {
  let mut found = Vec::new();
  for path in discover_local_mod_paths(mods_dir, Some(loader_type)) {
    if path
      .extension()
      .and_then(|ext| ext.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("disabled"))
    {
      continue;
    }
    // Forge only searches its version-specific child directory, not arbitrary folders.
    if path.parent() != Some(mods_dir)
      && path
        .parent()
        .and_then(Path::file_name)
        .is_none_or(|name| name != "1.7.10")
    {
      continue;
    }
    let metadata = (|| -> SJMCLResult<_> {
      let mut jar = ZipArchive::new(BufReader::new(File::open(&path)?))?;
      let bytes = read_entry(&mut jar, "mcmod.info", 1024 * 1024)?;
      read_legacy_forge_mod_metadata(bytes.as_slice())
    })();
    if let Ok(mods) = metadata {
      for metadata in mods {
        if metadata.modid == "lwjgl3ify" {
          found.push((path.clone(), metadata.version.unwrap_or_default()));
        }
      }
    }
  }
  if found.len() > 1 {
    return Err(compatibility_error(
      LaunchError::DuplicateLwjgl3ify,
      "Multiple enabled copies",
    ));
  }
  let Some((mod_path, version)) = found.pop() else {
    return Ok(None);
  };
  if version != SUPPORTED_VERSION {
    log::warn!(
      "Native lwjgl3ify support does not cover version {version}; using the original launch path"
    );
    return Ok(None);
  }
  let mut jar = ZipArchive::new(BufReader::new(File::open(&mod_path)?))?;
  let profile = (|| -> SJMCLResult<McClientInfo> {
    let bytes = read_entry(
      &mut jar,
      &format!("{RESOURCE_ROOT}version.json"),
      2 * 1024 * 1024,
    )?;
    Ok(serde_json::from_slice(&bytes)?)
  })()
  .map_err(|error| compatibility_error(InstanceError::ClientJsonParseError, error.0))?;
  let patch_name = format!("com.github.GTNewHorizons:lwjgl3ify:{version}:forgePatches");
  let mut patch = profile
    .libraries
    .iter()
    .find(|library| library.name == patch_name)
    .and_then(|library| library.downloads.as_ref()?.artifact.clone())
    .ok_or_else(|| {
      compatibility_error(
        InstanceError::ClientJsonParseError,
        "Missing forgePatches artifact",
      )
    })?;
  patch.path = safe_library_path(&patch_name, None)?;
  if patch.sha1.len() != 40 || !patch.sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
    return Err(compatibility_error(
      InstanceError::ClientJsonParseError,
      "Invalid forgePatches hash",
    ));
  }
  let info = Lwjgl3ifyLaunchInfo { mod_path, patch };
  Ok(Some((info, profile)))
}

fn safe_library_path(name: &str, classifier: Option<String>) -> SJMCLResult<String> {
  if name
    .split(':')
    .any(|part| part.is_empty() || part.contains(['/', '\\']) || part == "..")
  {
    return Err(compatibility_error(
      InstanceError::ClientJsonParseError,
      "Invalid Maven coordinate",
    ));
  }
  let path = convert_library_name_to_path(name, classifier)?;
  if !Path::new(&path)
    .components()
    .all(|part| matches!(part, Component::Normal(_)))
  {
    return Err(compatibility_error(
      InstanceError::ClientJsonParseError,
      "Invalid library path",
    ));
  }
  Ok(path)
}

fn platform_name(os: &str, arch: &str) -> SJMCLResult<String> {
  let base = match os {
    "windows" | "linux" => os,
    "macos" => "osx",
    _ => return Err(LaunchError::UnsupportedPlatform.into()),
  };
  Ok(match arch.to_ascii_lowercase().as_str() {
    "amd64" | "x86_64" | "x86-64" => base.to_string(),
    "aarch64" | "arm64" => format!("{base}-arm64"),
    "x86" | "i386" | "i486" | "i586" | "i686" if os == "windows" => base.to_string(),
    _ => return Err(LaunchError::UnsupportedPlatform.into()),
  })
}

// The bundled profile uses lwjgl3ify's extended OS names and ordered rule semantics.
// Evaluate these here without changing SJMCL's interpretation of other instances.
fn profile_library_allowed(library: &LibrariesValue, platform: &str) -> SJMCLResult<bool> {
  let mut allowed = library.rules.is_empty();
  for rule in &library.rules {
    if let Some(os) = &rule.os {
      if os.name != platform {
        continue;
      }
      if os.arch.is_some() || os.version.is_some() || rule.features.is_some() {
        return Err(InstanceError::ClientJsonParseError.into());
      }
    }
    allowed = match rule.action.as_str() {
      "allow" => true,
      "disallow" => false,
      _ => return Err(InstanceError::ClientJsonParseError.into()),
    };
  }
  Ok(allowed)
}

fn validate_custom_jvm_arguments(arguments: &str) -> SJMCLResult<()> {
  // Match command_generator's existing tokenization, including Windows backslashes.
  let arguments = arguments
    .split_whitespace()
    .map(str::to_string)
    .collect::<Vec<_>>();
  validate_jvm_values(&arguments, false)
}

fn validate_jvm_values(arguments: &[String], allow_managed_classpath: bool) -> SJMCLResult<()> {
  for (index, argument) in arguments.iter().enumerate() {
    if allow_managed_classpath && argument == "--class-path=${classpath}" {
      continue;
    }
    if allow_managed_classpath
      && matches!(argument.as_str(), "-cp" | "-classpath" | "--class-path")
      && arguments
        .get(index + 1)
        .is_some_and(|value| value == "${classpath}")
    {
      continue;
    }
    if (argument.starts_with("-Djava.system.class.loader=") && argument != RFB_LOADER)
      || matches!(
        argument.as_str(),
        "-cp" | "-classpath" | "--class-path" | "-jar"
      )
      || argument.starts_with("--class-path=")
    {
      return Err(SJMCLError(format!(
        "lwjgl3ify: JVM argument conflicts with the managed launch configuration: {argument}"
      )));
    }
  }
  Ok(())
}

pub fn apply_lwjgl3ify_profile(
  client_info: &mut McClientInfo,
  mut profile: McClientInfo,
  platform: &str,
) -> SJMCLResult<()> {
  if profile.main_class.as_deref() != Some(RFB_MAIN)
    || !profile.arguments.as_ref().is_some_and(|args| {
      args
        .jvm
        .iter()
        .any(|arg| arg.value.iter().any(|v| v == RFB_LOADER))
    })
  {
    return Err(InstanceError::ClientJsonParseError.into());
  }
  if client_info.main_class.as_deref().is_some_and(|main| {
    !matches!(
      main,
      "net.minecraft.launchwrapper.Launch" | "com.gtnewhorizons.retrofuturabootstrap.Main"
    ) && main != RFB_MAIN
  }) {
    return Err(SJMCLError(format!(
      "lwjgl3ify: Existing main class {:?} is incompatible with the RFB launch profile",
      client_info.main_class
    )));
  }
  if client_info.libraries.iter().any(|lib| {
    lib.name.starts_with("net.minecraftforge:forge:") && !lib.name.contains("10.13.4.1614")
  }) {
    return Err(SJMCLError(
      "lwjgl3ify: The bundled patches require Forge 10.13.4.1614".to_string(),
    ));
  }

  let mut libraries = Vec::new();
  for mut library in profile.libraries {
    if !profile_library_allowed(&library, platform)? {
      continue;
    }
    library.rules.clear();
    let path = safe_library_path(&library.name, None)?;
    let downloads = library
      .downloads
      .as_mut()
      .ok_or(InstanceError::ClientJsonParseError)?;
    if let Some(artifact) = &mut downloads.artifact {
      artifact.path = path;
    }
    if let Some(classifiers) = &mut downloads.classifiers {
      for (classifier, artifact) in classifiers {
        artifact.path = safe_library_path(&library.name, Some(classifier.clone()))?;
      }
    }
    libraries.push(library);
  }
  let patch_index = libraries
    .iter()
    .position(|lib| lib.name.ends_with(":forgePatches"))
    .ok_or(InstanceError::ClientJsonParseError)?;
  let patch = libraries.remove(patch_index);
  libraries.insert(0, patch);
  let existing = client_info
    .libraries
    .iter()
    .filter(|lib| {
      !lib.name.starts_with("org.lwjgl.lwjgl:")
        && !lib.name.starts_with("com.github.GTNewHorizons:lwjgl3ify:")
    })
    .cloned()
    .collect::<Vec<_>>();
  libraries = merge_library_lists(&libraries, &existing);

  let mut arguments = profile
    .arguments
    .take()
    .ok_or(InstanceError::ClientJsonParseError)?;
  let old_arguments = client_info.arguments.clone();
  if let Some(old) = &old_arguments {
    let values = old
      .jvm
      .iter()
      .flat_map(|argument| argument.value.iter().cloned())
      .collect::<Vec<_>>();
    validate_jvm_values(&values, true)?;
    let mut skip_classpath_value = false;
    let extra = old
      .jvm
      .iter()
      .cloned()
      .filter_map(|mut arg| {
        arg.value.retain(|value| {
          if skip_classpath_value {
            skip_classpath_value = false;
            return false;
          }
          if matches!(value.as_str(), "-cp" | "-classpath" | "--class-path") {
            skip_classpath_value = true;
            return false;
          }
          value != "--class-path=${classpath}"
        });
        (!arg.value.is_empty()).then_some(arg)
      })
      .filter(|arg| {
        !arguments
          .jvm
          .iter()
          .any(|existing| serde_json::to_value(existing).ok() == serde_json::to_value(arg).ok())
      })
      .collect::<Vec<_>>();
    arguments.jvm.extend(extra);
  }
  let mut game = if let Some(old) = old_arguments.filter(|args| !args.game.is_empty()) {
    old.game
  } else if let Some(legacy) = &client_info.minecraft_arguments {
    shlex::split(legacy)
      .ok_or(InstanceError::ClientJsonParseError)?
      .into_iter()
      .map(|value| ArgumentsItem {
        value: vec![value],
        ..Default::default()
      })
      .collect()
  } else {
    arguments.game.clone()
  };
  for argument in &arguments.game {
    if !argument.rules.is_empty()
      && !argument
        .value
        .iter()
        .any(|value| value.starts_with("--") && game.iter().any(|old| old.value.contains(value)))
    {
      game.push(argument.clone());
    }
  }
  if !game.iter().any(|arg| {
    arg
      .value
      .iter()
      .any(|value| value == "cpw.mods.fml.common.launcher.FMLTweaker")
  }) {
    game.push(ArgumentsItem {
      value: vec![
        "--tweakClass".to_string(),
        "cpw.mods.fml.common.launcher.FMLTweaker".to_string(),
      ],
      ..Default::default()
    });
  }
  arguments.game = game;
  client_info.main_class = profile.main_class;
  client_info.libraries = libraries;
  client_info.arguments = Some(arguments);
  // 25 is the recommendation in this bundled profile; the supported runtime floor is 17.
  let minimum = client_info
    .java_version
    .as_ref()
    .map_or(17, |java| java.major_version.max(17));
  client_info.java_version = Some(JavaVersion {
    component: String::new(),
    major_version: minimum,
  });
  log::info!("Using native launcher support for lwjgl3ify {SUPPORTED_VERSION} on {platform}");
  Ok(())
}

pub fn validate_lwjgl3ify_java(version: &str, minimum: i32) -> SJMCLResult<()> {
  let release = version.split('+').next().unwrap_or_default();
  let numeric = release.split('-').next().unwrap_or_default();
  let mut parts = numeric
    .split('.')
    .map(str::parse::<i32>)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| LaunchError::NoSuitableJava)?;
  let major_version = parts[0];
  if !(minimum.max(17)..=26).contains(&major_version) {
    return Err(LaunchError::NoSuitableJava.into());
  }
  if major_version == 17 {
    parts.resize(parts.len().max(3), 0);
    let below_minimum = parts.as_slice() < [17, 0, 6].as_slice();
    let minimum_prerelease =
      parts[..3] == [17, 0, 6] && parts[3..].iter().all(|part| *part == 0) && release.contains('-');
    if below_minimum || minimum_prerelease {
      return Err(LaunchError::NoSuitableJava.into());
    }
  }
  Ok(())
}

pub async fn prepare_lwjgl3ify(
  info: &Lwjgl3ifyLaunchInfo,
  libraries_dir: &Path,
) -> SJMCLResult<()> {
  let info = info.clone();
  let libraries_dir = libraries_dir.to_path_buf();
  tokio::task::spawn_blocking(move || prepare_patch(&info, &libraries_dir)).await?
}

fn prepare_patch(info: &Lwjgl3ifyLaunchInfo, libraries_dir: &Path) -> SJMCLResult<()> {
  let dest = libraries_dir.join(&info.patch.path);
  if validate_sha1(dest.clone(), info.patch.sha1.clone()).is_ok() {
    return Ok(());
  }
  let mut jar = ZipArchive::new(BufReader::new(File::open(&info.mod_path)?))?;
  let data = read_entry(
    &mut jar,
    &format!("{RESOURCE_ROOT}forgePatches.zip"),
    32 * 1024 * 1024,
  )?;
  if format!("{:x}", Sha1::digest(&data)) != info.patch.sha1 {
    return Err(SJMCLError(
      "Bundled forgePatches checksum mismatch".to_string(),
    ));
  }
  let parent = dest.parent().ok_or(InstanceError::ClientJsonParseError)?;
  fs::create_dir_all(parent)?;
  let temporary = parent.join(format!(".forgePatches-{}.tmp", uuid::Uuid::new_v4()));
  let result = (|| -> SJMCLResult<()> {
    let mut file = File::options()
      .write(true)
      .create_new(true)
      .open(&temporary)?;
    file.write_all(&data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &dest)?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(&temporary);
  }
  result
}

#[cfg(test)]
mod tests {
  use std::process::Command;
  use std::time::{Duration, Instant};

  #[cfg(target_os = "windows")]
  use std::os::windows::process::CommandExt;

  use crate::instance::helpers::client_json::{
    ArgumentsItem, DownloadsArtifact, InstructionRule, LaunchArgumentTemplate, LibrariesDownloads,
    LibrariesValue, McClientInfo, OsInfo,
  };
  use crate::launch::helpers::lwjgl3ify::{
    RFB_LOADER, RFB_MAIN, apply_lwjgl3ify_profile, java_platform_from_properties,
    java_probe_output, validate_lwjgl3ify_java,
  };

  fn probe_fixture_command(mode: &str) -> Command {
    let module = module_path!().split_once("::").unwrap().1;
    let fixture = format!("{module}::java_probe_fixture");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
      .args(["--exact", &fixture, "--nocapture"])
      .env("SJMCL_LWJGL3IFY_PROBE_TEST", mode);
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000);
    command
  }

  #[test]
  fn java_probe_fixture() {
    let Ok(mode) = std::env::var("SJMCL_LWJGL3IFY_PROBE_TEST") else {
      return;
    };
    println!("\njava.version = 26.0.1\nos.arch = amd64");
    println!("{}", "x".repeat(128 * 1024));
    eprintln!("{}", "x".repeat(128 * 1024));
    if mode == "hang" {
      std::thread::sleep(Duration::from_secs(3));
    }
  }

  #[test]
  fn java_probe_drains_both_output_pipes() {
    let properties =
      java_probe_output(&mut probe_fixture_command("normal"), Duration::from_secs(5)).unwrap();
    assert_eq!(
      java_platform_from_properties(&properties, "windows", 17).unwrap(),
      "windows"
    );
  }

  #[test]
  fn java_probe_timeout_terminates_and_reaps_the_child() {
    let started = Instant::now();
    assert!(
      java_probe_output(
        &mut probe_fixture_command("hang"),
        Duration::from_millis(100)
      )
      .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // A later candidate must still be able to complete normally.
    assert!(
      java_probe_output(&mut probe_fixture_command("normal"), Duration::from_secs(5)).is_ok()
    );
  }

  #[test]
  fn java_version_boundaries() {
    for version in [
      "17.0.6",
      "17.0.6+10-LTS",
      "17.0.6.1",
      "17.0.7-ea",
      "18",
      "21.0.8",
      "26",
    ] {
      assert!(validate_lwjgl3ify_java(version, 17).is_ok(), "{version}");
    }
    for version in [
      "",
      "invalid",
      "16.0.2",
      "17",
      "17.0.5+8",
      "17.0.6-ea+1",
      "27",
    ] {
      assert!(validate_lwjgl3ify_java(version, 17).is_err(), "{version}");
    }
    assert!(validate_lwjgl3ify_java("21.0.8", 25).is_err());
    assert!(validate_lwjgl3ify_java("25", 25).is_ok());
  }

  #[test]
  fn platform_uses_java_properties() {
    for (arch, expected) in [
      ("amd64", "windows"),
      ("x86_64", "windows"),
      ("aarch64", "windows-arm64"),
      ("arm64", "windows-arm64"),
      ("x86", "windows"),
    ] {
      let properties = format!("    java.version = 21.0.8\n    os.arch = {arch}\n");
      assert_eq!(
        java_platform_from_properties(&properties, "windows", 17).unwrap(),
        expected
      );
    }
  }

  #[test]
  fn incomplete_or_incompatible_java_is_rejected() {
    assert!(java_platform_from_properties("java.version = 21\n", "linux", 17).is_err());
    assert!(java_platform_from_properties("os.arch = amd64\n", "linux", 17).is_err());
    assert!(
      java_platform_from_properties("java.version = 17.0.5\nos.arch = amd64\n", "linux", 17)
        .is_err()
    );
    assert!(
      java_platform_from_properties("java.version = 21\nos.arch = riscv64\n", "linux", 17).is_err()
    );
  }

  #[test]
  fn profile_natives_follow_the_selected_java() {
    let library = |name: &str, os_names: &[&str]| LibrariesValue {
      name: name.to_string(),
      downloads: Some(LibrariesDownloads {
        artifact: Some(DownloadsArtifact::default()),
        ..Default::default()
      }),
      rules: os_names
        .iter()
        .map(|name| InstructionRule {
          action: "allow".to_string(),
          os: Some(OsInfo {
            name: name.to_string(),
            ..Default::default()
          }),
          ..Default::default()
        })
        .collect(),
      ..Default::default()
    };
    let x64 = "org.lwjgl:lwjgl-natives-macos:3.4.3";
    let arm64 = "org.lwjgl:lwjgl-natives-macos-arm64:3.4.3";
    for (arch, has_x64) in [("amd64", true), ("aarch64", false)] {
      let profile = McClientInfo {
        main_class: Some(RFB_MAIN.to_string()),
        arguments: Some(LaunchArgumentTemplate {
          jvm: vec![ArgumentsItem {
            value: vec![RFB_LOADER.to_string()],
            ..Default::default()
          }],
          ..Default::default()
        }),
        libraries: vec![
          library(
            "com.github.GTNewHorizons:lwjgl3ify:3.0.37:forgePatches",
            &[],
          ),
          library(x64, &["osx"]),
          library(arm64, &["osx", "osx-arm64"]),
        ],
        ..Default::default()
      };
      let properties = format!("java.version = 21.0.8\nos.arch = {arch}\n");
      let platform = java_platform_from_properties(&properties, "macos", 17).unwrap();
      let mut client = McClientInfo::default();
      apply_lwjgl3ify_profile(&mut client, profile, &platform).unwrap();
      assert_eq!(client.libraries.iter().any(|lib| lib.name == x64), has_x64);
      assert!(client.libraries.iter().any(|lib| lib.name == arm64));
      assert!(client.libraries[0].name.ends_with(":forgePatches"));
    }
  }
}
