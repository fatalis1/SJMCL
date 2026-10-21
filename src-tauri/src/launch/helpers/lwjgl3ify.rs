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
  let (java, arch) = loop {
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
      platform_name(std::env::consts::OS, &arch)?;
      Ok((java, arch))
    }
    .await;
    match result {
      Ok(selected) => break selected,
      Err(error) if game_config.game_java.auto && requirement > 17 => {
        log::warn!(
          "lwjgl3ify: Java >= {requirement} runtime selection failed ({error:?}); retrying with minimum Java 17"
        );
        requirement = 17;
      }
      Err(error) => return Err(error),
    }
  };
  let profile = apply_profile(client_info, profile, &arch)?;
  game_config.advanced.jvm.args =
    sanitize_managed_jvm_arguments(split_jvm_args(&game_config.advanced.jvm.args)).join(" ");
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
  Ok(Some((profile, java)))
}

// Only errors from identified lwjgl3ify archives should stop the launch.
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
  if let Ok(executable) = tokio::fs::canonicalize(exec_path).await
    && let Some(home) = executable.parent().and_then(Path::parent)
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
  let profile_name = format!("{RESOURCE_ROOT}version.json");
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
    let Ok(file) = File::open(&path) else {
      continue;
    };
    let Ok(mut jar) = ZipArchive::new(BufReader::new(file)) else {
      continue;
    };
    let has_profile = jar.index_for_name(&profile_name).is_some();
    let metadata = (|| -> SJMCLResult<_> {
      let entry = match jar.by_name("mcmod.info") {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
      };
      let bytes = read_entry(entry, "mcmod.info", 1024 * 1024)?;
      if !has_profile
        && !bytes
          .windows(b"lwjgl3ify".len())
          .any(|window| window == b"lwjgl3ify")
      {
        return Ok(None);
      }
      Ok(
        parse_legacy_forge_mod_metadata_bytes(&bytes)?
          .into_iter()
          .find(|metadata| metadata.modid == "lwjgl3ify"),
      )
    })();
    let metadata = match metadata {
      Ok(metadata) => metadata,
      Err(error) if has_profile => return Err(error),
      Err(_) => continue,
    };
    if !has_profile && metadata.is_none() {
      continue;
    }
    if found.is_some() {
      return Err(SJMCLError(
        "Multiple enabled copies of lwjgl3ify".to_string(),
      ));
    }
    found = Some((
      path,
      metadata
        .and_then(|metadata| metadata.version)
        .unwrap_or_else(|| "unknown".to_string()),
    ));
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
  // Normalize every option without crossing rule boundaries, and strip FML tweakers.
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
  let mut normalized = Vec::new();
  while let Some((index, value)) = values.next() {
    if value == "--tweakClass" && values.peek().is_some_and(|(_, next)| next == FML_TWEAKER) {
      values.next();
      continue;
    }
    if let Some(tweaker) = value.strip_prefix("--tweakClass=") {
      if tweaker != FML_TWEAKER {
        normalized.push(ArgumentsItem {
          value: vec!["--tweakClass".to_string(), tweaker.to_string()],
          rules: arguments[index].rules.clone(),
        });
      }
      continue;
    }
    let mut argument = ArgumentsItem {
      value: vec![value],
      rules: arguments[index].rules.clone(),
    };
    if argument.value[0].starts_with('-')
      && !argument.value[0].contains('=')
      && values.peek().is_some_and(|(next_index, next)| {
        arguments[*next_index].rules == argument.rules && !next.starts_with('-')
      })
    {
      argument.value.push(values.next().unwrap().1);
    }
    normalized.push(argument);
  }
  normalized
}

fn normalize_forge_version(version: &str) -> &str {
  let version = version.strip_prefix("1.7.10-").unwrap_or(version);
  version.strip_suffix("-1.7.10").unwrap_or(version)
}

fn is_forge_library(name: &str) -> bool {
  name.starts_with("net.minecraftforge:forge:")
    || name.starts_with("net.minecraftforge:minecraftforge:")
}

// Keep quoting and Windows backslashes intact while consuming whole option operands.
fn split_jvm_args(raw: &str) -> Vec<String> {
  let mut arguments = Vec::new();
  let mut token = String::new();
  let mut quote = None;
  let mut chars = raw.char_indices();
  while let Some((index, ch)) = chars.next() {
    if quote.is_none() && ch.is_ascii_whitespace() {
      if !token.is_empty() {
        arguments.push(std::mem::take(&mut token));
      }
      continue;
    }
    token.push(ch);
    if quote == Some('"')
      && ch == '\\'
      && let Some(remaining) = raw[index + 1..].strip_prefix('"')
    {
      let starts_flag = remaining.starts_with(|ch: char| ch.is_ascii_whitespace())
        && remaining
          .trim_start_matches(|ch: char| ch.is_ascii_whitespace())
          .starts_with('-');
      // A later closing quote must belong to this value, before another quoted assignment.
      if !starts_flag
        && remaining
          .split("=\"")
          .next()
          .unwrap_or_default()
          .contains('"')
      {
        token.push('"');
        chars.next();
        continue;
      }
    }
    if quote == Some(ch) {
      quote = None;
    } else if quote.is_none() && matches!(ch, '\'' | '"') {
      quote = Some(ch);
    }
  }
  if !token.is_empty() {
    arguments.push(token);
  }
  arguments
}

// The embedded profile owns the classpath, native library path, entry point and system classloader.
fn sanitize_managed_jvm_arguments(arguments: Vec<String>) -> Vec<String> {
  let mut result = Vec::new();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    let unquoted = argument.trim_matches(['\'', '"']);
    if matches!(unquoted, "-cp" | "-classpath" | "--class-path" | "-jar") {
      arguments.next();
    } else if [
      "-cp=",
      "-classpath=",
      "--class-path=",
      "-jar=",
      "-Djava.system.class.loader=",
      "-Djava.library.path=",
    ]
    .iter()
    .any(|prefix| unquoted.starts_with(prefix))
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

  let platform = platform_name(std::env::consts::OS, java_arch)?;
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
    .filter(|library| is_forge_library(&library.name))
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
    .filter(|library| is_forge_library(&library.name))
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
  let feature = FeaturesInfo::default();
  let mut existing = Vec::new();
  for library in &original.libraries {
    if !library.is_allowed(&feature)?
      || library.name.starts_with("org.lwjgl.lwjgl:")
      || library.name.starts_with("org.lwjgl:")
      || is_forge_library(&library.name)
      || library
        .name
        .starts_with("com.github.GTNewHorizons:lwjgl3ify:")
      || library.name.starts_with("me.eigenraven.lwjgl3ify:")
    {
      continue;
    }
    existing.push(library.clone());
  }
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
  let has_original_game = original
    .arguments
    .as_ref()
    .is_some_and(|args| !args.game.is_empty())
    || original.minecraft_arguments.is_some();
  let mut metadata_game = metadata_game.into_iter().peekable();
  while let Some(argument) = metadata_game.next() {
    if has_original_game
      && let Some(flag) = argument
        .value
        .first()
        .filter(|value| value.starts_with('-') && value.split('=').next() != Some("--tweakClass"))
      && game
        .iter()
        .flat_map(|item| &item.value)
        .any(|value| value.split('=').next() == flag.split('=').next())
    {
      if argument.value.len() == 1 {
        if !flag.contains('=')
          && metadata_game.peek().is_some_and(|next| {
            next.rules == argument.rules && next.value.len() == 1 && !next.value[0].starts_with('-')
          })
        {
          metadata_game.next();
        }
        continue;
      }
      if argument.value.len() == 2 && !flag.contains('=') && !argument.value[1].starts_with('-') {
        continue;
      }
    }
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
