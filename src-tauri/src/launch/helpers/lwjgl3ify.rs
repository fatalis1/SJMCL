use sjmcl_types::error::{SJMCLError, SJMCLResult};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path};
use tauri::AppHandle;
use zip::{ZipArchive, result::ZipError};

use crate::instance::helpers::client_json::{
  ArgumentsItem, DownloadsArtifact, FeaturesInfo, IsAllowed, JavaVersion, LibrariesValue,
  McClientInfo,
};
use crate::instance::helpers::misc::get_instance_subdir_paths;
use crate::instance::helpers::mods::common::discover_local_mod_paths;
use crate::instance::helpers::mods::legacy_forge::parse_legacy_forge_mod_metadata_bytes;
use crate::instance::models::misc::{Instance, InstanceError, InstanceSubdirType, ModLoaderType};
use crate::launch::helpers::file_validator::{
  convert_library_name_to_path, merge_library_lists, parse_library_name,
};
use crate::launch::helpers::jre_selector::select_java_runtime;
use crate::launcher_config::models::{GameConfig, JavaInfo};

const RESOURCE_ROOT: &str = "me/eigenraven/lwjgl3ify/relauncher/";
const RFB_MAIN: &str = "com.gtnewhorizons.retrofuturabootstrap.MainStartOnFirstThread";
const RFB_MAIN_LEGACY: &str = "com.gtnewhorizons.retrofuturabootstrap.Main";
const RFB_LOADER: &str =
  "-Djava.system.class.loader=com.gtnewhorizons.retrofuturabootstrap.RfbSystemClassLoader";
const FML_TWEAKER: &str = "cpw.mods.fml.common.launcher.FMLTweaker";

pub async fn try_apply_lwjgl3ify(
  app: &AppHandle,
  instance: &Instance,
  game_config: &mut GameConfig,
  client_info: &McClientInfo,
) -> SJMCLResult<Option<(McClientInfo, JavaInfo)>> {
  let Some((version, profile)) = resolve_lwjgl3ify(app, instance).await? else {
    return Ok(None);
  };
  let mut requirement = profile
    .java_version
    .as_ref()
    .map_or(17, |java| java.major_version.max(17));
  loop {
    let result: SJMCLResult<_> = async {
      let java =
        select_java_runtime(app, Some(&game_config.game_java), instance, requirement).await?;
      if java.major_version < 17 {
        return Err(SJMCLError(format!(
          "lwjgl3ify requires Java 17 or newer; selected Java {}",
          java.major_version
        )));
      }
      let arch = detect_java_arch(&java.exec_path).await?;
      let applied = apply_profile(client_info, profile.clone(), &arch)?;
      Ok((applied, java, arch))
    }
    .await;
    match result {
      Ok((profile, java, arch)) => {
        game_config.advanced.jvm.args = sanitize_managed_jvm_arguments(
          game_config
            .advanced
            .jvm
            .args
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        )
        .join(" ");
        log::info!(
          "lwjgl3ify: version={version}, Java={}, arch={}, main_class={:?}, first_libraries={:?}",
          java.major_version,
          arch,
          profile.main_class,
          profile
            .libraries
            .iter()
            .take(3)
            .map(|library| library.name.as_str())
            .collect::<Vec<_>>()
        );
        return Ok(Some((profile, java)));
      }
      Err(error) if game_config.game_java.auto && requirement > 17 => {
        log::warn!(
          "lwjgl3ify: Java >= {requirement} launch profile failed ({error:?}); retrying with minimum Java 17"
        );
        requirement = 17;
      }
      Err(error) => return Err(error),
    }
  }
}

// Absence is optional; invalid enabled mods must stop the launch.
async fn resolve_lwjgl3ify(
  app: &AppHandle,
  instance: &Instance,
) -> SJMCLResult<Option<(String, McClientInfo)>> {
  if !is_candidate(&instance.version, instance.mod_loader.loader_type) {
    return Ok(None);
  }
  let dirs = get_instance_subdir_paths(app, instance, &[&InstanceSubdirType::Mods])
    .ok_or(InstanceError::InstanceNotFoundByID)?;
  let mods_dir = dirs[0].clone();
  let loader_type = instance.mod_loader.loader_type;
  let candidate =
    tokio::task::spawn_blocking(move || find_lwjgl3ify(&mods_dir, loader_type)).await??;
  if candidate.is_none() {
    log::debug!("lwjgl3ify: no relaunch profile found; using original launch profile");
  }
  Ok(candidate)
}

fn is_candidate(version: &str, loader: ModLoaderType) -> bool {
  version == "1.7.10" && matches!(loader, ModLoaderType::Forge | ModLoaderType::LegacyForge)
}

fn normalize_java_arch(arch: &str) -> String {
  match arch.trim().to_ascii_lowercase().as_str() {
    "amd64" | "x64" | "x86-64" | "x86_64" => "x86_64".to_string(),
    "arm64" | "aarch64" => "aarch64".to_string(),
    "i386" | "i486" | "i586" | "i686" | "x86" => "x86".to_string(),
    "arm32" | "armv6" | "armv6l" | "armv7" | "armv7l" | "armv8l" | "arm" => "arm".to_string(),
    "ppc64le" | "powerpc64le" => "powerpc64le".to_string(),
    "ppc64" | "powerpc64" => "powerpc64".to_string(),
    other => other.to_string(),
  }
}

async fn detect_java_arch(exec_path: &str) -> SJMCLResult<String> {
  // Resolve symlinks such as /usr/bin/java before looking for the target JVM's release file.
  let executable = tokio::fs::canonicalize(exec_path).await?;
  if let Some(home) = executable.parent().and_then(Path::parent)
    && let Ok(release) = tokio::fs::read_to_string(home.join("release")).await
  {
    for line in release.lines() {
      if let Some((key, value)) = line.split_once('=')
        && key.trim() == "OS_ARCH"
      {
        let arch = value.trim().trim_matches('"');
        if !arch.is_empty() {
          return Ok(normalize_java_arch(arch));
        }
      }
    }
  }

  // Tokio's process feature is not enabled; keep the subprocess local to this compatibility layer.
  let command_path = exec_path.to_string();
  let output = tokio::task::spawn_blocking(move || {
    let mut command = std::process::Command::new(command_path);
    command.args(["-XshowSettings:properties", "-version"]);
    #[cfg(target_os = "windows")]
    {
      use std::os::windows::process::CommandExt;
      command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.output()
  })
  .await??;
  if output.status.success() {
    for bytes in [&output.stderr, &output.stdout] {
      for line in String::from_utf8_lossy(bytes).lines() {
        if let Some((key, value)) = line.split_once('=')
          && key.trim() == "os.arch"
          && !value.trim().is_empty()
        {
          return Ok(normalize_java_arch(value));
        }
      }
    }
  }
  Err(SJMCLError(format!(
    "Cannot detect Java architecture: {exec_path}"
  )))
}

fn platform_name(os: &str, arch: &str) -> SJMCLResult<&'static str> {
  match (os, arch) {
    ("windows", "aarch64") => Ok("windows-arm64"),
    ("windows", "x86_64") => Ok("windows"),
    ("macos", "aarch64") => Ok("osx-arm64"),
    ("macos", "x86_64") => Ok("osx"),
    ("linux", "aarch64") => Ok("linux-arm64"),
    ("linux", "arm") => Ok("linux-arm32"),
    ("linux", "powerpc64le") => Ok("linux-ppc64le"),
    ("linux", "x86_64") => Ok("linux"),
    ("freebsd", "x86_64") => Ok("freebsd"),
    _ => Err(SJMCLError(format!(
      "Unsupported lwjgl3ify platform: {os}/{arch}"
    ))),
  }
}

// version.json encodes arch in os.name (osx-arm64/windows-arm64); evaluate here, then clear library.rules for SJMCL.
fn profile_library_allowed(library: &LibrariesValue, platform: &str) -> SJMCLResult<bool> {
  let mut allowed = library.rules.is_empty();
  for rule in &library.rules {
    if rule.features.is_some()
      || rule
        .os
        .as_ref()
        .is_some_and(|os| os.arch.is_some() || os.version.is_some())
    {
      return Err(SJMCLError(format!(
        "Unsupported lwjgl3ify library rule constraints: {}",
        library.name
      )));
    }
    if rule.os.as_ref().is_some_and(|os| os.name != platform) {
      continue;
    }
    allowed = match rule.action.as_str() {
      "allow" => true,
      "disallow" => false,
      _ => {
        return Err(SJMCLError(format!(
          "Unknown library rule action: {}",
          rule.action
        )));
      }
    };
  }
  Ok(allowed)
}

fn read_entry(mut entry: impl Read, name: &str, limit: u64) -> SJMCLResult<Vec<u8>> {
  let mut contents = Vec::new();
  entry.by_ref().take(limit + 1).read_to_end(&mut contents)?;
  if contents.len() as u64 > limit {
    return Err(SJMCLError(format!("Oversized lwjgl3ify resource: {name}")));
  }
  Ok(contents)
}

fn find_lwjgl3ify(
  mods_dir: &Path,
  loader: ModLoaderType,
) -> SJMCLResult<Option<(String, McClientInfo)>> {
  let mut found = None;
  for path in discover_local_mod_paths(mods_dir, Some(loader)) {
    if !path
      .extension()
      .and_then(|ext| ext.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("zip"))
    {
      continue;
    }
    if path.parent() != Some(mods_dir) && path.parent() != Some(mods_dir.join("1.7.10").as_path()) {
      continue;
    }
    let mut jar = ZipArchive::new(BufReader::new(File::open(&path)?))?;
    let metadata = match jar.by_name("mcmod.info") {
      Ok(entry) => read_entry(entry, "mcmod.info", 1024 * 1024)?,
      Err(ZipError::FileNotFound) => continue,
      Err(error) => return Err(error.into()),
    };
    if let Some(metadata) = parse_legacy_forge_mod_metadata_bytes(&metadata)?
      .into_iter()
      .find(|metadata| metadata.modid == "lwjgl3ify")
    {
      if found.is_some() {
        return Err(SJMCLError(
          "Multiple enabled copies of lwjgl3ify".to_string(),
        ));
      }
      found = Some((
        path,
        metadata.version.unwrap_or_else(|| "unknown".to_string()),
      ));
    }
  }
  let Some((path, version)) = found else {
    return Ok(None);
  };
  let mut jar = ZipArchive::new(BufReader::new(File::open(path)?))?;
  let name = format!("{RESOURCE_ROOT}version.json");
  let entry = match jar.by_name(&name) {
    Ok(entry) => entry,
    Err(ZipError::FileNotFound) => {
      log::info!(
        "lwjgl3ify: version={version} uses the legacy launch mechanism; keeping original launch profile"
      );
      return Ok(None);
    }
    Err(error) => return Err(error.into()),
  };
  let bytes = read_entry(entry, &name, 2 * 1024 * 1024)?;
  Ok(Some((version, serde_json::from_slice(&bytes)?)))
}

fn safe_library_path(name: &str, classifier: Option<String>) -> SJMCLResult<String> {
  let base_coord = name.split('@').next().unwrap_or_default();
  let colon_parts = base_coord.split(':').count();
  // Maven coordinates have three or four parts and at most one extension suffix.
  if !(3..=4).contains(&colon_parts) || name.matches('@').count() > 1 {
    return Err(SJMCLError("Invalid Maven coordinate".to_string()));
  }
  // An embedded classifier must not override a separate download classifier.
  if colon_parts == 4 && classifier.is_some() {
    return Err(SJMCLError("Invalid Maven coordinate".to_string()));
  }
  // Coordinate parts must stay within their own path components.
  let valid_component = |part: &str| {
    !part.is_empty()
      && !matches!(part, "." | "..")
      && part
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
  };
  if name.split([':', '@']).any(|part| !valid_component(part))
    || base_coord
      .split(':')
      .next()
      .unwrap_or_default()
      .split('.')
      .any(|part| !valid_component(part))
  {
    return Err(SJMCLError("Invalid Maven coordinate".to_string()));
  }
  // A separate classifier cannot contain coordinate or path separators.
  if classifier
    .as_ref()
    .is_some_and(|part| !valid_component(part))
  {
    return Err(SJMCLError("Invalid Maven coordinate".to_string()));
  }
  let path = convert_library_name_to_path(name, classifier)?;
  if !Path::new(&path)
    .components()
    .all(|part| matches!(part, Component::Normal(_)))
  {
    return Err(SJMCLError("Invalid library path".to_string()));
  }
  Ok(path)
}

fn validate_rules(arguments: &[ArgumentsItem]) -> SJMCLResult<()> {
  for argument in arguments {
    for rule in &argument.rules {
      rule.is_allowed(&FeaturesInfo::default())?;
    }
  }
  Ok(())
}

fn active_jvm(arguments: &[ArgumentsItem]) -> SJMCLResult<Vec<String>> {
  validate_rules(arguments)?;
  let mut result = Vec::new();
  for argument in arguments {
    if argument.is_allowed(&FeaturesInfo::default())? {
      result.extend(argument.value.clone());
    }
  }
  Ok(result)
}

fn remove_fml_tweakers(mut arguments: Vec<ArgumentsItem>) -> Vec<ArgumentsItem> {
  // Keep each value's original group so its rules and position stay intact.
  let values = arguments
    .iter_mut()
    .enumerate()
    .flat_map(|(index, argument)| {
      std::mem::take(&mut argument.value)
        .into_iter()
        .map(move |value| (index, value))
    })
    .collect::<Vec<_>>();
  let mut values = values.into_iter().peekable();
  while let Some((index, value)) = values.next() {
    if value == "--tweakClass" && values.peek().is_some_and(|(_, next)| next == FML_TWEAKER) {
      values.next();
      continue;
    } else if value.strip_prefix("--tweakClass=") == Some(FML_TWEAKER) {
      continue;
    }
    arguments[index].value.push(value);
  }
  arguments.retain(|argument| !argument.value.is_empty());
  arguments
}

fn normalize_forge_version(version: &str) -> &str {
  let version = version.strip_prefix("1.7.10-").unwrap_or(version);
  version.strip_suffix("-1.7.10").unwrap_or(version)
}

// The embedded profile owns the classpath, entry point and system classloader.
fn sanitize_managed_jvm_arguments(arguments: Vec<String>) -> Vec<String> {
  let mut result = Vec::new();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    if matches!(
      argument.as_str(),
      "-cp" | "-classpath" | "--class-path" | "-jar"
    ) {
      arguments.next();
    } else if [
      "-cp=",
      "-classpath=",
      "--class-path=",
      "-jar=",
      "-Djava.system.class.loader=",
    ]
    .iter()
    .any(|prefix| argument.starts_with(prefix))
    {
      continue;
    } else {
      result.push(argument);
    }
  }
  result
}

fn apply_profile(
  original: &McClientInfo,
  mut profile: McClientInfo,
  java_arch: &str,
) -> SJMCLResult<McClientInfo> {
  if !matches!(
    profile.main_class.as_deref(),
    Some(RFB_MAIN | RFB_MAIN_LEGACY)
  ) || !matches!(
    original.main_class.as_deref(),
    Some("net.minecraft.launchwrapper.Launch" | RFB_MAIN | RFB_MAIN_LEGACY)
  ) {
    return Err(SJMCLError("Unrecognized launch entry point".to_string()));
  }
  let mut arguments = profile
    .arguments
    .take()
    .ok_or_else(|| SJMCLError("Missing JVM arguments".to_string()))?;
  let managed_jvm = active_jvm(&arguments.jvm)?;
  if arguments
    .jvm
    .iter()
    .flat_map(|argument| &argument.value)
    .filter(|value| value.starts_with("-Djava.system.class.loader="))
    .count()
    != 1
    || !managed_jvm.iter().any(|value| value == RFB_LOADER)
    || managed_jvm
      .iter()
      .filter(|value| value.starts_with("-Djava.library.path="))
      .count()
      != 1
  {
    return Err(SJMCLError(
      "Missing, duplicate or disabled critical RFB JVM arguments".to_string(),
    ));
  }
  validate_rules(&arguments.game)?;
  // CLASSPATH is managed by SJMCL's existing command generator/environment.
  let mut managed = Vec::new();
  let mut values = managed_jvm.into_iter();
  while let Some(value) = values.next() {
    if matches!(value.as_str(), "-cp" | "-classpath" | "--class-path") {
      if values.next().as_deref() != Some("${classpath}") {
        return Err(SJMCLError("Invalid metadata classpath".to_string()));
      }
    } else if [
      "-cp=${classpath}",
      "-classpath=${classpath}",
      "--class-path=${classpath}",
    ]
    .contains(&value.as_str())
    {
      continue;
    } else if ["-cp=", "-classpath=", "--class-path=", "-jar="]
      .iter()
      .any(|prefix| value.starts_with(prefix))
      || value == "-jar"
    {
      return Err(SJMCLError(
        "Conflicting metadata entry point/classpath".to_string(),
      ));
    } else {
      managed.push(value);
    }
  }

  // Tauri groups FreeBSD under Linux in type_().
  let os = if cfg!(target_os = "freebsd") {
    "freebsd".to_string()
  } else {
    tauri_plugin_os::type_().to_string()
  };
  let platform = platform_name(&os, java_arch)?;
  let mut libraries = Vec::new();
  for mut library in profile.libraries {
    if !profile_library_allowed(&library, platform)? {
      continue;
    }
    // The normal downloader/classpath evaluator only recognizes standard OS names.
    library.rules.clear();
    let path = safe_library_path(&library.name, None)?;
    if library.downloads.as_ref().is_none_or(|downloads| {
      downloads.artifact.is_none()
        && downloads
          .classifiers
          .as_ref()
          .is_none_or(|classifiers| classifiers.is_empty())
    }) {
      let is_patch = parse_library_name(&library.name, None)?
        .classifier
        .as_deref()
        == Some("forgePatches");
      let repository = library
        .url
        .as_deref()
        .or((!is_patch).then_some("https://libraries.minecraft.net/"));
      if let Some(repository) = repository {
        let mut base = url::Url::parse(repository)?;
        if !matches!(base.scheme(), "https" | "http") || !base.has_host() {
          return Err(SJMCLError("Invalid Maven repository URL".to_string()));
        }
        // Append Maven path components without treating reserved characters as URL syntax.
        base
          .path_segments_mut()
          .map_err(|_| SJMCLError("Invalid Maven repository URL".to_string()))?
          .pop_if_empty()
          .extend(path.split('/'));
        let downloads = library.downloads.get_or_insert_with(Default::default);
        downloads.artifact = Some(DownloadsArtifact {
          url: base.to_string(),
          ..Default::default()
        });
      }
    }
    if let Some(downloads) = &mut library.downloads {
      if let Some(artifact) = &mut downloads.artifact {
        artifact.path = path;
      }
      if let Some(classifiers) = &mut downloads.classifiers {
        for (classifier, artifact) in classifiers {
          artifact.path = safe_library_path(&library.name, Some(classifier.clone()))?;
        }
      }
    }
    libraries.push(library);
  }
  let patches = libraries
    .iter()
    .filter(|library| {
      parse_library_name(&library.name, None)
        .is_ok_and(|parts| parts.classifier.as_deref() == Some("forgePatches"))
    })
    .collect::<Vec<_>>();
  if patches.len() != 1 {
    return Err(SJMCLError(
      "Missing unique forgePatches library".to_string(),
    ));
  }
  if !patches[0]
    .downloads
    .as_ref()
    .and_then(|d| d.artifact.as_ref())
    .is_some_and(|artifact| {
      url::Url::parse(&artifact.url).is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
    })
  {
    return Err(SJMCLError(
      "Missing downloadable forgePatches artifact".to_string(),
    ));
  }
  let patch_name = patches[0].name.clone();
  let forge_versions = libraries
    .iter()
    .filter(|library| library.name.starts_with("net.minecraftforge:forge:"))
    .map(|library| parse_library_name(&library.name, None))
    .collect::<SJMCLResult<Vec<_>>>()?;
  if forge_versions.len() != 1 || forge_versions[0].classifier.as_deref() != Some("universal") {
    return Err(SJMCLError(
      "Missing unique Forge universal coordinate".to_string(),
    ));
  }
  let forge_version = &forge_versions[0].pack_version;
  let original_forge = original
    .libraries
    .iter()
    .filter(|library| library.name.starts_with("net.minecraftforge:forge:"))
    .map(|library| parse_library_name(&library.name, None))
    .collect::<SJMCLResult<Vec<_>>>()?;
  if original_forge.is_empty() {
    return Err(SJMCLError("Missing original Forge coordinate".to_string()));
  }
  for forge in original_forge {
    let actual = forge.pack_version;
    if normalize_forge_version(&actual) != normalize_forge_version(forge_version) {
      return Err(SJMCLError(format!(
        "Forge version mismatch: expected {forge_version}, actual {actual}"
      )));
    }
  }
  let existing = original
    .libraries
    .iter()
    .filter(|library| {
      !library.name.starts_with("org.lwjgl.lwjgl:")
        && !library.name.starts_with("org.lwjgl:")
        && !library.name.starts_with("net.minecraftforge:forge:")
        && !library
          .name
          .starts_with("com.github.GTNewHorizons:lwjgl3ify:")
    })
    .cloned()
    .collect::<Vec<_>>();
  let mut libraries = merge_library_lists(&libraries, &existing);
  let patch_index = libraries
    .iter()
    .position(|library| library.name == patch_name)
    .ok_or_else(|| SJMCLError("forgePatches lost during library merge".to_string()))?;
  let patch = libraries.remove(patch_index);
  libraries.insert(0, patch);

  let old_jvm = original
    .arguments
    .as_ref()
    .map(|args| active_jvm(&args.jvm))
    .transpose()?
    .unwrap_or_default();
  let old_jvm_count = old_jvm.len();
  let mut jvm = sanitize_managed_jvm_arguments(old_jvm);
  let discarded_jvm_count = old_jvm_count - jvm.len();
  jvm.extend(managed);
  arguments.jvm = jvm
    .into_iter()
    .map(|value| ArgumentsItem {
      value: vec![value],
      ..Default::default()
    })
    .collect();
  let metadata_game = remove_fml_tweakers(arguments.game);
  let mut game = if let Some(old) = original
    .arguments
    .as_ref()
    .filter(|args| !args.game.is_empty())
  {
    validate_rules(&old.game)?;
    remove_fml_tweakers(old.game.clone())
  } else if let Some(legacy) = &original.minecraft_arguments {
    remove_fml_tweakers(
      legacy
        .split_whitespace()
        .map(str::to_string)
        .map(|value| ArgumentsItem {
          value: vec![value],
          ..Default::default()
        })
        .collect(),
    )
  } else {
    metadata_game.clone()
  };
  for argument in metadata_game {
    if !game.contains(&argument) {
      game.push(argument);
    }
  }
  // Install one unconditional FML tweaker before any other tweakers, even if all originals were ruled out.
  game.insert(
    0,
    ArgumentsItem {
      value: vec!["--tweakClass".to_string(), FML_TWEAKER.to_string()],
      ..Default::default()
    },
  );
  arguments.game = game;
  let java = profile.java_version.take().unwrap_or_default();
  let mut result = original.clone();
  result.main_class = profile.main_class;
  result.arguments = Some(arguments);
  result.minecraft_arguments = None;
  result.libraries = libraries;
  result.java_version = Some(JavaVersion {
    major_version: java.major_version.max(17),
    ..java
  });
  log::info!("lwjgl3ify: discarded_original_jvm_arguments={discarded_jvm_count}");
  Ok(result)
}

#[cfg(test)]
mod tests {
  use crate::instance::helpers::client_json::{InstructionRule, LaunchArgumentTemplate, OsInfo};
  use crate::launch::helpers::file_validator::get_invalid_library_files;
  use crate::launch::helpers::lwjgl3ify::*;
  use crate::resource::models::SourceType;
  use crate::tasks::PTaskParam;
  use std::io::Write;
  use std::path::PathBuf;
  use zip::write::SimpleFileOptions;

  struct TestDir(PathBuf);

  impl TestDir {
    fn new() -> Self {
      let path = std::env::temp_dir().join(format!("sjmcl-lwjgl3ify-{}", uuid::Uuid::new_v4()));
      std::fs::create_dir_all(&path).unwrap();
      Self(path)
    }
  }

  impl Drop for TestDir {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  fn write_mod(path: &Path, metadata: Option<&[u8]>, profile: Option<&[u8]>) {
    let mut jar = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, bytes) in [
      ("mcmod.info".to_string(), metadata),
      (format!("{RESOURCE_ROOT}version.json"), profile),
    ] {
      if let Some(bytes) = bytes {
        jar.start_file(name, SimpleFileOptions::default()).unwrap();
        jar.write_all(bytes).unwrap();
      }
    }
    jar.finish().unwrap();
  }

  fn library(name: &str) -> LibrariesValue {
    LibrariesValue {
      name: name.to_string(),
      ..Default::default()
    }
  }

  fn profiles() -> (McClientInfo, McClientInfo) {
    let original = McClientInfo {
      main_class: Some("net.minecraft.launchwrapper.Launch".to_string()),
      minecraft_arguments: Some(format!(
        r"--gameDir C:\Games\Minecraft --flag -single --tweakClass {FML_TWEAKER} --tweakClass {FML_TWEAKER}"
      )),
      libraries: vec![
        library("net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10"),
        library("org.lwjgl.lwjgl:lwjgl:2.9.1"),
        library("org.lwjgl:lwjgl:3.1"),
        library("com.github.GTNewHorizons:lwjgl3ify:1.0"),
        library("example:keep:1.0"),
      ],
      ..Default::default()
    };
    let profile = serde_json::from_value(serde_json::json!({
      "mainClass": RFB_MAIN,
      "javaVersion": { "majorVersion": 21 },
      "arguments": {
        "jvm": [RFB_LOADER, "-Djava.library.path=${natives_directory}", "-cp", "${classpath}"],
        "game": ["--tweakClass", FML_TWEAKER, "--profile-flag", "-independent"]
      },
      "libraries": [
        { "name": "net.minecraftforge:forge:1.7.10-10.13.4.1614:universal" },
        { "name": "com.github.GTNewHorizons:lwjgl3ify:2.1:forgePatches", "url": "https://example.invalid/maven" },
        { "name": "example:keep:2.0" }
      ]
    })).unwrap();
    (original, profile)
  }

  #[test]
  fn discovers_by_modid_and_ignores_missing_metadata_disabled_and_other_versions() {
    let dir = TestDir::new();
    write_mod(&dir.0.join("lwjgl3ify-compat.jar"), None, None);
    write_mod(
      &dir.0.join("lwjgl3ify-unrelated.jar"),
      Some(br#"[{"modid":"other"}]"#),
      None,
    );
    write_mod(
      &dir.0.join("lwjgl3ify.jar.disabled"),
      Some(br#"[{"modid":"lwjgl3ify"}]"#),
      None,
    );
    std::fs::create_dir(dir.0.join("1.12.2")).unwrap();
    write_mod(
      &dir.0.join("1.12.2/lwjgl3ify.jar"),
      Some(br#"[{"modid":"lwjgl3ify"}]"#),
      None,
    );
    assert!(
      find_lwjgl3ify(&dir.0, ModLoaderType::Forge)
        .unwrap()
        .is_none()
    );
    std::fs::create_dir(dir.0.join("1.7.10")).unwrap();
    write_mod(
      &dir.0.join("1.7.10/renamed.zip"),
      Some(br#"{"modList":[{"modid":"lwjgl3ify","version":"custom-build"}]}"#),
      Some(b"{}"),
    );
    assert_eq!(
      find_lwjgl3ify(&dir.0, ModLoaderType::LegacyForge)
        .unwrap()
        .unwrap()
        .0,
      "custom-build"
    );
    write_mod(
      &dir.0.join("duplicate.jar"),
      Some(br#"[{"modid":"lwjgl3ify"}]"#),
      None,
    );
    assert_eq!(
      find_lwjgl3ify(&dir.0, ModLoaderType::Forge).unwrap_err().0,
      "Multiple enabled copies of lwjgl3ify"
    );
  }

  #[test]
  fn detects_legacy_by_resource_absence_and_rejects_bad_or_oversized_profiles() {
    let dir = TestDir::new();
    let path = dir.0.join("lwjgl3ify.jar");
    let metadata = br#"[{"modid":"lwjgl3ify","version":"99.0-custom"}]"#;
    write_mod(&path, Some(metadata), None);
    assert!(
      find_lwjgl3ify(&dir.0, ModLoaderType::Forge)
        .unwrap()
        .is_none()
    );
    write_mod(&path, Some(metadata), Some(b"not json"));
    assert!(find_lwjgl3ify(&dir.0, ModLoaderType::Forge).is_err());
    write_mod(
      &path,
      Some(metadata),
      Some(&vec![b' '; 2 * 1024 * 1024 + 1]),
    );
    assert!(
      find_lwjgl3ify(&dir.0, ModLoaderType::Forge)
        .unwrap_err()
        .0
        .contains("Oversized")
    );
  }

  #[tokio::test]
  async fn reads_target_java_architecture_from_release_file() {
    let dir = TestDir::new();
    std::fs::create_dir(dir.0.join("bin")).unwrap();
    let executable = dir.0.join("bin/java");
    std::fs::write(&executable, b"").unwrap();
    std::fs::write(
      dir.0.join("release"),
      "JAVA_VERSION=\"21\"\nOS_ARCH=\"aarch64\"\n",
    )
    .unwrap();
    assert_eq!(
      detect_java_arch(executable.to_str().unwrap())
        .await
        .unwrap(),
      "aarch64"
    );
    for (arch, expected) in [
      ("AMD64", "x86_64"),
      ("arm64", "aarch64"),
      ("i686", "x86"),
      ("armv7l", "arm"),
      ("ppc64le", "powerpc64le"),
    ] {
      assert_eq!(normalize_java_arch(arch), expected);
    }
    for os in ["windows", "macos", "linux", "freebsd"] {
      assert!(platform_name(os, "x86").is_err());
    }
    assert_eq!(
      platform_name("windows", "aarch64").unwrap(),
      "windows-arm64"
    );
    assert_eq!(
      platform_name("linux", "powerpc64le").unwrap(),
      "linux-ppc64le"
    );
  }

  #[test]
  fn evaluates_custom_platform_rules_and_rejects_unsupported_constraints() {
    let mut lib = library("org.lwjgl:lwjgl:3.3.6:natives-windows-arm64");
    lib.rules = vec![InstructionRule {
      action: "allow".to_string(),
      os: Some(OsInfo {
        name: "windows-arm64".to_string(),
        ..Default::default()
      }),
      ..Default::default()
    }];
    assert!(profile_library_allowed(&lib, "windows-arm64").unwrap());
    assert!(!profile_library_allowed(&lib, "windows").unwrap());
    lib.rules[0].os.as_mut().unwrap().arch = Some("aarch64".to_string());
    assert!(profile_library_allowed(&lib, "windows-arm64").is_err());
    lib.rules[0].os.as_mut().unwrap().arch = None;
    lib.rules[0].features = Some(FeaturesInfo::default());
    assert!(profile_library_allowed(&lib, "windows-arm64").is_err());
  }

  #[test]
  fn applies_profile_without_losing_windows_paths_or_argument_boundaries() {
    let (mut original, profile) = profiles();
    original.arguments = Some(LaunchArgumentTemplate {
      jvm: vec![ArgumentsItem {
        value: vec![
          "-classpath".to_string(),
          "old.jar".to_string(),
          "-Djava.system.class.loader=Old".to_string(),
          "-Xmx2G".to_string(),
        ],
        ..Default::default()
      }],
      ..Default::default()
    });
    let result = apply_profile(&original, profile, "x86_64").unwrap();
    assert!(result.libraries[0].name.ends_with(":forgePatches"));
    let patch = result.libraries[0]
      .downloads
      .as_ref()
      .unwrap()
      .artifact
      .as_ref()
      .unwrap();
    assert_eq!(
      patch.url,
      "https://example.invalid/maven/com/github/GTNewHorizons/lwjgl3ify/2.1/lwjgl3ify-2.1-forgePatches.jar"
    );
    assert!(
      !result
        .libraries
        .iter()
        .any(|library| library.name.starts_with("org.lwjgl"))
    );
    assert_eq!(
      result
        .libraries
        .iter()
        .filter(|library| library.name.starts_with("example:keep:"))
        .count(),
      1
    );
    let args = result.arguments.unwrap();
    let jvm = active_jvm(&args.jvm).unwrap();
    assert_eq!(
      jvm
        .iter()
        .filter(|value| value.starts_with("-Djava.system.class.loader="))
        .count(),
      1
    );
    assert!(jvm.contains(&"-Xmx2G".to_string()));
    assert!(!jvm.contains(&"old.jar".to_string()));
    let game = args
      .game
      .iter()
      .flat_map(|item| &item.value)
      .collect::<Vec<_>>();
    assert_eq!(
      game
        .iter()
        .filter(|value| value.as_str() == FML_TWEAKER)
        .count(),
      1
    );
    assert!(
      game
        .iter()
        .any(|value| value.as_str() == r"C:\Games\Minecraft")
    );
    assert!(args.game.iter().any(|item| item.value == ["-independent"]));
    assert!(args.game.iter().any(|item| item.value == ["-single"]));
    assert!(result.minecraft_arguments.is_none());
  }

  #[test]
  fn rejects_mismatched_forge_missing_or_duplicate_patches_and_unsafe_coordinates() {
    let (original, mut profile) = profiles();
    profile.libraries[0].name =
      "net.minecraftforge:forge:1.7.10-10.13.4.1448:universal".to_string();
    assert!(
      apply_profile(&original, profile, "x86_64")
        .unwrap_err()
        .0
        .contains("mismatch")
    );
    let (_, mut profile) = profiles();
    profile.libraries.remove(1);
    assert!(apply_profile(&original, profile, "x86_64").is_err());
    let (_, mut profile) = profiles();
    profile.libraries.push(profile.libraries[1].clone());
    assert!(apply_profile(&original, profile, "x86_64").is_err());
    for name in [
      "../evil:lib:1",
      "org..evil:lib:1",
      "org:../lib:1",
      "org:lib:..",
      "org:lib:1@jar@zip",
      "org:lib:1:../../evil",
      "org:lib:1?query",
    ] {
      assert!(safe_library_path(name, None).is_err(), "{name}");
    }
  }

  #[test]
  fn sanitizes_all_managed_entry_point_options_and_preserves_other_jvm_arguments() {
    let input = "-Xmx2G -cp old.jar -classpath other.jar --class-path third.jar -jar game.jar -cp=one --class-path=two -jar=three -Djava.system.class.loader=Other -Dkeep=true";
    assert_eq!(
      sanitize_managed_jvm_arguments(input.split_whitespace().map(str::to_string).collect()),
      ["-Xmx2G", "-Dkeep=true"]
    );
  }

  #[test]
  fn merges_library_versions_at_the_first_insertion_position() {
    let merged = merge_library_lists(
      &[library("example:patch:1.0"), library("example:other:1.0")],
      &[library("example:patch:2.0"), library("example:last:1.0")],
    );
    assert_eq!(
      merged
        .iter()
        .map(|lib| lib.name.as_str())
        .collect::<Vec<_>>(),
      ["example:patch:2.0", "example:other:1.0", "example:last:1.0"]
    );
  }

  #[tokio::test]
  async fn redownloads_empty_truncated_and_html_checksumless_archives() {
    let dir = TestDir::new();
    let mut lib = library("example:library:1.0");
    lib.downloads = Some(crate::instance::helpers::client_json::LibrariesDownloads {
      artifact: Some(DownloadsArtifact {
        path: "library.jar".to_string(),
        url: "https://libraries.minecraft.net/example/library/1.0/library-1.0.jar".to_string(),
        ..Default::default()
      }),
      ..Default::default()
    });
    let client = McClientInfo {
      libraries: vec![lib],
      ..Default::default()
    };
    let path = dir.0.join("library.jar");
    for bytes in [
      b"".as_slice(),
      b"<html>404 Not Found</html>",
      b"PK\x03\x04truncated",
    ] {
      std::fs::write(&path, bytes).unwrap();
      for check_hash in [false, true] {
        let tasks = get_invalid_library_files(SourceType::Official, &dir.0, &client, check_hash)
          .await
          .unwrap();
        assert_eq!(tasks.len(), 1);
        let PTaskParam::Download(download) = &tasks[0];
        assert!(download.sha1.is_none());
      }
    }
    write_mod(&path, Some(b"[]"), None);
    assert!(
      get_invalid_library_files(SourceType::Official, &dir.0, &client, true)
        .await
        .unwrap()
        .is_empty()
    );
  }
}
