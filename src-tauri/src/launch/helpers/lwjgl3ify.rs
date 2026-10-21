use sjmcl_types::error::{SJMCLError, SJMCLResult};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Component, Path};
use tauri::AppHandle;
use zip::ZipArchive;

use crate::instance::helpers::client_json::{
  ArgumentsItem, FeaturesInfo, IsAllowed, JavaVersion, LibrariesValue, McClientInfo,
};
use crate::instance::helpers::misc::get_instance_subdir_paths;
use crate::instance::helpers::mods::common::discover_local_mod_paths;
use crate::instance::helpers::mods::legacy_forge::read_legacy_forge_mod_metadata;
use crate::instance::models::misc::{Instance, InstanceError, InstanceSubdirType, ModLoaderType};
use crate::launch::helpers::file_validator::{
  convert_library_name_to_path, merge_library_lists, parse_library_name,
};
use crate::launcher_config::models::JavaInfo;

const RESOURCE_ROOT: &str = "me/eigenraven/lwjgl3ify/relauncher/";
const RFB_MAIN: &str = "com.gtnewhorizons.retrofuturabootstrap.MainStartOnFirstThread";
const RFB_MAIN_LEGACY: &str = "com.gtnewhorizons.retrofuturabootstrap.Main";
const RFB_LOADER: &str =
  "-Djava.system.class.loader=com.gtnewhorizons.retrofuturabootstrap.RfbSystemClassLoader";
const FML_TWEAKER: &str = "cpw.mods.fml.common.launcher.FMLTweaker";

// A failed optional takeover never changes the original instance's launch profile.
pub async fn resolve_lwjgl3ify(
  app: &AppHandle,
  instance: &Instance,
) -> Option<(String, McClientInfo)> {
  if !is_candidate(&instance.version, instance.mod_loader.loader_type) {
    return None;
  }
  let result = async {
    let dirs = get_instance_subdir_paths(app, instance, &[&InstanceSubdirType::Mods])
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    let mods_dir = dirs[0].clone();
    let loader_type = instance.mod_loader.loader_type;
    tokio::task::spawn_blocking(move || resolve_from_mods(&mods_dir, loader_type))
      .await
      .map_err(SJMCLError::from)
  }
  .await;
  match result {
    Ok(candidate) => candidate,
    Err(error) => {
      log::warn!("lwjgl3ify: takeover failed; using original launch profile: {error:?}");
      None
    }
  }
}

fn is_candidate(version: &str, loader: ModLoaderType) -> bool {
  version == "1.7.10" && matches!(loader, ModLoaderType::Forge | ModLoaderType::LegacyForge)
}

fn resolve_from_mods(mods_dir: &Path, loader: ModLoaderType) -> Option<(String, McClientInfo)> {
  match find_lwjgl3ify(mods_dir, loader) {
    Ok(Some(profile)) => Some(profile),
    Ok(None) => {
      log::debug!("lwjgl3ify: no enabled mod found; using original launch profile");
      None
    }
    Err(error) => {
      log::warn!("lwjgl3ify: takeover failed; using original launch profile: {error:?}");
      None
    }
  }
}

pub fn lwjgl3ify_java_requirement(javas: &[JavaInfo], preferred: i32) -> i32 {
  let preferred = preferred.max(17);
  if javas.iter().any(|java| java.major_version >= preferred) {
    return preferred;
  }
  log::warn!("lwjgl3ify: no scanned Java >= {preferred}; lowering the selection minimum to 17");
  17
}

pub fn accept_lwjgl3ify_java(
  has_metadata: bool,
  selection: SJMCLResult<JavaInfo>,
) -> SJMCLResult<(JavaInfo, bool)> {
  let java = selection?;
  if has_metadata && java.major_version < 17 {
    log::warn!(
      "lwjgl3ify: selected Java {} is below 17; using original launch profile",
      java.major_version
    );
    return Ok((java, false));
  }
  Ok((java, has_metadata))
}

fn platform_name(os: &str, arch: &str) -> SJMCLResult<&'static str> {
  match (os, arch) {
    ("windows", "aarch64") => Ok("windows-arm64"),
    ("windows", "x86" | "x86_64") => Ok("windows"),
    ("macos", "aarch64") => Ok("osx-arm64"),
    ("macos", "x86" | "x86_64") => Ok("osx"),
    ("linux", "aarch64") => Ok("linux-arm64"),
    ("linux", "arm") => Ok("linux-arm32"),
    ("linux", "powerpc64le") => Ok("linux-ppc64le"),
    ("linux", "x86" | "x86_64") => Ok("linux"),
    ("freebsd", _) => Ok("freebsd"),
    _ => Err(SJMCLError(format!(
      "Unsupported lwjgl3ify platform: {os}/{arch}"
    ))),
  }
}

fn profile_library_allowed(library: &LibrariesValue, platform: &str) -> SJMCLResult<bool> {
  let mut allowed = library.rules.is_empty();
  for rule in &library.rules {
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
  loader: ModLoaderType,
) -> SJMCLResult<Option<(String, McClientInfo)>> {
  let mut found = None;
  for path in discover_local_mod_paths(mods_dir, Some(loader)) {
    if !path
      .file_name()
      .and_then(|name| name.to_str())
      .is_some_and(|name| name.to_ascii_lowercase().contains("lwjgl3ify"))
    {
      continue;
    }
    if path
      .extension()
      .and_then(|ext| ext.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("disabled"))
    {
      continue;
    }
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
    let mods = match metadata {
      Ok(mods) => mods,
      Err(error) => {
        log::warn!(
          "lwjgl3ify: skipping unreadable mod metadata in {}: {error:?}",
          path.display()
        );
        continue;
      }
    };
    for metadata in mods {
      if metadata.modid == "lwjgl3ify" {
        if found.is_some() {
          return Err(SJMCLError(
            "Multiple enabled copies of lwjgl3ify".to_string(),
          ));
        }
        found = Some((
          path.clone(),
          metadata.version.unwrap_or_else(|| "unknown".to_string()),
        ));
      }
    }
  }
  let Some((path, version)) = found else {
    return Ok(None);
  };
  let mut jar = ZipArchive::new(BufReader::new(File::open(path)?))?;
  let bytes = read_entry(
    &mut jar,
    &format!("{RESOURCE_ROOT}version.json"),
    2 * 1024 * 1024,
  )?;
  Ok(Some((version, serde_json::from_slice(&bytes)?)))
}

fn safe_library_path(name: &str, classifier: Option<String>) -> SJMCLResult<String> {
  if !(3..=4).contains(
    &name
      .split('@')
      .next()
      .unwrap_or_default()
      .split(':')
      .count(),
  ) || name.matches('@').count() > 1
    || (classifier.is_some()
      && name
        .split('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .count()
        == 4)
    || name
      .split([':', '@'])
      .any(|part| part.is_empty() || part.contains(['/', '\\']) || part == "..")
    || classifier
      .as_ref()
      .is_some_and(|part| part.is_empty() || part.contains(['/', '\\', ':', '@']) || part == "..")
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

// JVM argument values are discrete strings; discard conflicting switches and operands.
pub fn sanitize_lwjgl3ify_jvm(arguments: Vec<String>) -> Vec<String> {
  let mut result = Vec::new();
  let mut args = arguments.into_iter();
  while let Some(argument) = args.next() {
    if matches!(
      argument.as_str(),
      "-cp" | "-classpath" | "--class-path" | "-jar"
    ) {
      log::warn!("lwjgl3ify: discarding user JVM option {argument} and its operand");
      args.next();
    } else if ["-cp=", "-classpath=", "--class-path=", "-jar="]
      .iter()
      .any(|prefix| argument.starts_with(prefix))
    {
      log::warn!("lwjgl3ify: discarding user JVM option {argument}");
    } else if argument.starts_with("-Djava.system.class.loader=") {
      if argument != RFB_LOADER {
        log::warn!("lwjgl3ify: discarding user classloader override {argument}");
      }
      // The validated metadata supplies the single authoritative RFB classloader.
    } else {
      result.push(argument);
    }
  }
  result
}

fn strip_tweak_classes(arguments: Vec<ArgumentsItem>) -> Vec<ArgumentsItem> {
  let mut removed = HashSet::new();
  let mut values = arguments
    .iter()
    .flat_map(|argument| &argument.value)
    .enumerate()
    .peekable();
  while let Some((index, value)) = values.next() {
    if value == "--tweakClass"
      && values
        .peek()
        .is_some_and(|(_, next)| next.as_str() == FML_TWEAKER)
    {
      removed.insert(index);
      removed.insert(values.next().unwrap().0);
    } else if value.strip_prefix("--tweakClass=") == Some(FML_TWEAKER)
      || (value == "--tweakClass" && values.peek().is_none())
    {
      removed.insert(index);
    }
  }
  let mut index = 0;
  arguments
    .into_iter()
    .filter_map(|mut argument| {
      argument.value.retain(|_| {
        let keep = !removed.contains(&index);
        index += 1;
        keep
      });
      (!argument.value.is_empty()).then_some(argument)
    })
    .collect()
}

fn normalize_forge_version(version: &str) -> &str {
  let version = version.strip_prefix("1.7.10-").unwrap_or(version);
  version.strip_suffix("-1.7.10").unwrap_or(version)
}

pub(crate) fn apply_profile(
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
    || managed_jvm
      .iter()
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
  if patches.len() != 1
    || !patches[0]
      .downloads
      .as_ref()
      .and_then(|d| d.artifact.as_ref())
      .is_some_and(|artifact| {
        url::Url::parse(&artifact.url).is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
      })
  {
    return Err(SJMCLError(
      "Missing unique downloadable forgePatches library".to_string(),
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
  for library in &original.libraries {
    if library.name.starts_with("net.minecraftforge:forge:") {
      let actual = parse_library_name(&library.name, None)?.pack_version;
      if normalize_forge_version(&actual) != normalize_forge_version(forge_version) {
        return Err(SJMCLError(format!(
          "Forge version mismatch: expected {forge_version}, actual {actual}"
        )));
      }
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
  let mut jvm = sanitize_lwjgl3ify_jvm(old_jvm);
  let discarded_jvm_count = old_jvm_count - jvm.len();
  jvm.extend(managed);
  arguments.jvm = jvm
    .into_iter()
    .map(|value| ArgumentsItem {
      value: vec![value],
      ..Default::default()
    })
    .collect();
  let metadata_game = strip_tweak_classes(arguments.game);
  let mut game = if let Some(old) = original
    .arguments
    .as_ref()
    .filter(|args| !args.game.is_empty())
  {
    validate_rules(&old.game)?;
    strip_tweak_classes(old.game.clone())
  } else if let Some(legacy) = &original.minecraft_arguments {
    strip_tweak_classes(
      shlex::split(legacy)
        .ok_or_else(|| SJMCLError("Invalid legacy game arguments".to_string()))?
        .into_iter()
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
    if !argument.rules.is_empty()
      && !argument
        .value
        .iter()
        .any(|value| value.starts_with("--") && game.iter().any(|old| old.value.contains(value)))
    {
      game.push(argument);
    }
  }
  game.push(ArgumentsItem {
    value: vec!["--tweakClass".to_string(), FML_TWEAKER.to_string()],
    ..Default::default()
  });
  arguments.game = game;
  let java = profile
    .java_version
    .take()
    .ok_or_else(|| SJMCLError("Missing metadata Java requirement".to_string()))?;
  let mut result = original.clone();
  result.main_class = profile.main_class;
  result.arguments = Some(arguments);
  result.minecraft_arguments = None;
  result.libraries = libraries;
  result.java_version = Some(JavaVersion {
    major_version: java.major_version.max(17),
    ..java
  });
  log::info!("lwjgl3ify: discarded_user_jvm_arguments={discarded_jvm_count}");
  Ok(result)
}
