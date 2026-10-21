use sjmcl_types::error::{SJMCLError, SJMCLResult};
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Component, Path};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use zip::ZipArchive;

use crate::instance::helpers::client_json::{
  ArgumentsItem, DownloadsArtifact, FeaturesInfo, IsAllowed, JavaVersion, LibrariesValue,
  McClientInfo,
};
use crate::instance::helpers::misc::get_instance_subdir_paths;
use crate::instance::helpers::mods::common::discover_local_mod_paths;
use crate::instance::helpers::mods::legacy_forge::read_legacy_forge_mod_metadata;
use crate::instance::models::misc::{Instance, InstanceError, InstanceSubdirType, ModLoaderType};
use crate::launch::helpers::file_validator::{
  convert_library_name_to_path, merge_library_lists, parse_library_name,
};
use crate::launch::helpers::jre_selector::select_java_runtime;
use crate::launch::helpers::misc::sanitize_managed_jvm_arguments;
use crate::launcher_config::models::{GameJava, JavaInfo};

const RESOURCE_ROOT: &str = "me/eigenraven/lwjgl3ify/relauncher/";
const RFB_MAIN: &str = "com.gtnewhorizons.retrofuturabootstrap.MainStartOnFirstThread";
const RFB_MAIN_LEGACY: &str = "com.gtnewhorizons.retrofuturabootstrap.Main";
const RFB_LOADER: &str =
  "-Djava.system.class.loader=com.gtnewhorizons.retrofuturabootstrap.RfbSystemClassLoader";
const FML_TWEAKER: &str = "cpw.mods.fml.common.launcher.FMLTweaker";

pub async fn try_apply_lwjgl3ify(
  app: &AppHandle,
  instance: &Instance,
  game_java: &GameJava,
  client_info: &McClientInfo,
) -> SJMCLResult<Option<(McClientInfo, JavaInfo)>> {
  if let Some((version, profile)) = resolve_lwjgl3ify(app, instance).await {
    let mut requirement = profile
      .java_version
      .as_ref()
      .map_or(17, |java| java.major_version.max(17));
    if game_java.auto {
      let javas = app.state::<Mutex<Vec<JavaInfo>>>();
      requirement = lwjgl3ify_java_requirement(&javas.lock()?, requirement);
    }
    match select_java_runtime(app, Some(game_java), instance, requirement).await {
      Ok(java) if java.major_version >= 17 => {
        match apply_profile(client_info, profile, &java.arch) {
          Ok(profile) => {
            log::info!(
              "lwjgl3ify: version={version}, Java={}, arch={}, main_class={:?}, first_libraries={:?}",
              java.major_version,
              java.arch,
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
          Err(error) => log::warn!(
            "lwjgl3ify: profile application failed ({error:?}); retrying original launch profile"
          ),
        }
      }
      Ok(java) => log::warn!(
        "lwjgl3ify: selected Java {} is below 17; retrying original launch profile",
        java.major_version
      ),
      Err(error) => {
        log::warn!("lwjgl3ify: Java selection failed ({error:?}); retrying original launch profile")
      }
    }
  }
  Ok(None)
}

// A failed optional takeover never changes the original instance's launch profile.
async fn resolve_lwjgl3ify(app: &AppHandle, instance: &Instance) -> Option<(String, McClientInfo)> {
  if !is_candidate(&instance.version, instance.mod_loader.loader_type) {
    return None;
  }
  let result = async {
    let dirs = get_instance_subdir_paths(app, instance, &[&InstanceSubdirType::Mods])
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    let mods_dir = dirs[0].clone();
    let loader_type = instance.mod_loader.loader_type;
    tokio::task::spawn_blocking(move || find_lwjgl3ify(&mods_dir, loader_type))
      .await
      .map_err(SJMCLError::from)?
  }
  .await;
  match result {
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

fn is_candidate(version: &str, loader: ModLoaderType) -> bool {
  version == "1.7.10" && matches!(loader, ModLoaderType::Forge | ModLoaderType::LegacyForge)
}

fn lwjgl3ify_java_requirement(javas: &[JavaInfo], preferred: i32) -> i32 {
  let preferred = preferred.max(17);
  if javas.iter().any(|java| java.major_version >= preferred) {
    return preferred;
  }
  log::warn!("lwjgl3ify: no scanned Java >= {preferred}; lowering the selection minimum to 17");
  17
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

// version.json encodes arch in os.name (osx-arm64/windows-arm64); evaluate here, then clear library.rules for SJMCL.
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
    if path.parent() != Some(mods_dir) && path.parent() != Some(mods_dir.join("1.7.10").as_path()) {
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
  if name
    .split([':', '@'])
    .any(|part| part.is_empty() || part.contains(['/', '\\']) || part == "..")
  {
    return Err(SJMCLError("Invalid Maven coordinate".to_string()));
  }
  // A separate classifier cannot contain coordinate or path separators.
  if classifier
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

fn clean_tweak_classes(
  mut arguments: Vec<ArgumentsItem>,
  mut keep_fml: bool,
) -> Vec<ArgumentsItem> {
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
    if value == "--tweakClass" {
      match values.peek() {
        Some((_, next)) if next == FML_TWEAKER => {
          let (_, next) = values.next().unwrap();
          if keep_fml {
            arguments[index].value.extend([value, next]);
            keep_fml = false;
          }
          continue;
        }
        None => continue,
        _ => {}
      }
    } else if value.strip_prefix("--tweakClass=") == Some(FML_TWEAKER) {
      if !keep_fml {
        continue;
      }
      keep_fml = false;
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
        .or_else(|| (!is_patch).then_some("https://libraries.minecraft.net/"));
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
  let mut jvm = sanitize_managed_jvm_arguments(old_jvm, &managed);
  let discarded_jvm_count = old_jvm_count - jvm.len();
  jvm.extend(managed);
  arguments.jvm = jvm
    .into_iter()
    .map(|value| ArgumentsItem {
      value: vec![value],
      ..Default::default()
    })
    .collect();
  let metadata_game = clean_tweak_classes(arguments.game, false);
  let mut game = if let Some(old) = original
    .arguments
    .as_ref()
    .filter(|args| !args.game.is_empty())
  {
    validate_rules(&old.game)?;
    clean_tweak_classes(old.game.clone(), true)
  } else if let Some(legacy) = &original.minecraft_arguments {
    clean_tweak_classes(
      shlex::split(legacy)
        .ok_or_else(|| SJMCLError("Invalid legacy game arguments".to_string()))?
        .into_iter()
        .map(|value| ArgumentsItem {
          value: vec![value],
          ..Default::default()
        })
        .collect(),
      true,
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
  if !game.iter().any(|argument| {
    argument
      .value
      .windows(2)
      .any(|pair| pair[0] == "--tweakClass" && pair[1] == FML_TWEAKER)
      || argument
        .value
        .iter()
        .any(|value| value.strip_prefix("--tweakClass=") == Some(FML_TWEAKER))
  }) {
    game.push(ArgumentsItem {
      value: vec!["--tweakClass".to_string(), FML_TWEAKER.to_string()],
      ..Default::default()
    });
  }
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
  log::info!("lwjgl3ify: discarded_original_jvm_arguments={discarded_jvm_count}");
  Ok(result)
}
