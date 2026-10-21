use lazy_static;
use regex::Regex;
use std::collections::HashMap;

// A managed profile owns the classpath, entry point and system classloader.
pub fn sanitize_managed_jvm_arguments(
  arguments: Vec<String>,
  managed_args: &[String],
) -> Vec<String> {
  let mut result = Vec::new();
  let mut args = arguments.into_iter();
  while let Some(argument) = args.next() {
    if matches!(
      argument.as_str(),
      "-cp" | "-classpath" | "--class-path" | "-jar"
    ) {
      log::warn!("Discarding JVM option {argument} and its operand: managed launch profile");
      args.next();
    } else if ["-cp=", "-classpath=", "--class-path=", "-jar="]
      .iter()
      .any(|prefix| argument.starts_with(prefix))
    {
      log::warn!("Discarding JVM option {argument}: managed launch profile");
    } else if argument.starts_with("-Djava.system.class.loader=") {
      if !managed_args.contains(&argument) {
        log::warn!("Discarding JVM option {argument}: managed launch profile");
      }
    } else {
      result.push(argument);
    }
  }
  result
}

pub fn replace_arguments(args: Vec<String>, map: &HashMap<String, String>) -> Vec<String> {
  lazy_static::lazy_static!(
    static ref PARAM_REGEX: Regex = Regex::new(r"\$\{([^}]+)\}").unwrap();
  );
  let mut cmd = Vec::new();
  for arg in args {
    let mut replaced_arg = arg.clone();
    let mut unknown_arg = false;

    for caps in PARAM_REGEX.captures_iter(&arg) {
      let arg_name = &caps[1];
      match map.get(arg_name) {
        Some(value) => {
          replaced_arg = replaced_arg.replacen(&caps[0], value, 1);
        }
        None => {
          unknown_arg = true;
          break;
        }
      }
    }
    if !unknown_arg {
      cmd.push(replaced_arg);
    } else {
      cmd.push(arg);
    }
  }
  cmd
}

pub fn get_natives_string(natives: &HashMap<String, String>) -> Option<String> {
  let target_os = cfg_select! {
    target_os = "windows" => "windows",
    target_os = "linux" => "linux",
    target_os = "macos" => "osx",
    _ => "other",
  };
  if let Some(native) = natives.get(target_os) {
    let mut map = HashMap::<String, String>::new();
    let arch = std::mem::size_of::<usize>() * 8;
    map.insert("arch".to_string(), arch.to_string());
    replace_arguments(vec![native.clone()], &map).pop()
  } else {
    None
  }
}

pub fn get_separator() -> &'static str {
  cfg_select! {
    windows => ";",
    _ => ":",
  }
}
