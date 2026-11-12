//! Explicit discovery of local commands. No version probes or shell execution.
use crate::config::CustomCli;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct DiscoveredCli {
    pub name: String,
    pub command: PathBuf,
}

pub fn discovery_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    let home = crate::ide_detector::IdeDetector::get_user_home();
    paths.extend([
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".local/share/ai-ensemble-cli/command"),
        crate::ide_detector::IdeDetector::get_local_appdata().join("Programs/AIEnsemble/command"),
    ]);
    paths
}

fn runnable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| {
                ["exe", "com", "cmd", "bat", "ps1"].contains(&value.to_ascii_lowercase().as_str())
            })
            .unwrap_or(false)
    }
}

pub fn discover_in(paths: &[PathBuf]) -> Vec<DiscoveredCli> {
    let mut seen_dirs = HashSet::new();
    let mut seen_commands = HashSet::new();
    let mut found = Vec::new();
    // A click must not enumerate unbounded mounts or return an unbounded UI list.
    let mut budget = 16_384usize;
    for directory in paths {
        if !directory.is_absolute() || !seen_dirs.insert(directory.clone()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let command = entry.path();
            if !runnable_file(&command) {
                continue;
            }
            let Some(filename) = command.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            #[cfg(target_os = "windows")]
            let name = command
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(filename)
                .to_string();
            #[cfg(not(target_os = "windows"))]
            let name = filename.to_string();
            candidates.push(DiscoveredCli { name, command });
        }
        // Prefer native executables to npm's paired .cmd/.ps1 wrappers.
        candidates.sort_by_key(|entry| {
            (
                entry
                    .command
                    .extension()
                    .map(|ext| ext != "exe")
                    .unwrap_or(true),
                entry.command.clone(),
            )
        });
        for candidate in candidates {
            let key = if cfg!(target_os = "windows") {
                candidate.name.to_lowercase()
            } else {
                candidate.name.clone()
            };
            if seen_commands.insert(key) {
                found.push(candidate);
            }
        }
        if budget == 0 {
            break;
        }
    }
    found.sort_by_key(|entry| entry.name.to_lowercase());
    found
}

pub fn resolve_command(command: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    let path = PathBuf::from(command.trim().trim_matches('"'));
    if path.is_absolute() {
        return runnable_file(&path).then_some(path);
    }
    if path.components().count() != 1 {
        return None;
    }
    discover_in(paths)
        .into_iter()
        .find(|entry| {
            if cfg!(target_os = "windows") {
                entry.name.eq_ignore_ascii_case(command.trim())
            } else {
                entry.name == command.trim()
            }
        })
        .map(|entry| entry.command)
}

pub fn prepare(
    name: &str,
    command: &str,
    instructions: &str,
    existing: &[CustomCli],
) -> Result<CustomCli, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err("Укажите название CLI.".into());
    }
    let command = resolve_command(command, &discovery_paths())
        .ok_or("Команда не обнаружена. Выберите результат поиска или полный путь к CLI.")?;
    if existing
        .iter()
        .any(|entry| entry.name.eq_ignore_ascii_case(name) || entry.command == command)
    {
        return Err("CLI с таким названием или командой уже добавлен.".into());
    }
    let instructions_path = if instructions.trim().is_empty() {
        None
    } else {
        let text = instructions.trim();
        let path =
            if let Some(relative) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
                crate::ide_detector::IdeDetector::get_user_home().join(relative)
            } else {
                PathBuf::from(text)
            };
        if !valid_instructions_path(&path) {
            return Err(
                "Файл инструкций: полный путь к .md или .mdc (также поддерживается ~/).".into(),
            );
        }
        if existing.iter().any(|entry| {
            entry
                .instructions_path
                .as_deref()
                .map(|target| same_path(target, &path))
                .unwrap_or(false)
        }) {
            return Err("Этот файл инструкций уже используется другим CLI.".into());
        }
        Some(path)
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    Ok(CustomCli {
        id: format!("cli-{stamp}"),
        name: name.into(),
        command,
        instructions_path,
        enabled: false,
    })
}

pub fn valid_instructions_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| ["md", "mdc"].contains(&value.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
}

pub fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    if cfg!(target_os = "windows") {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

pub fn is_detected(cli: &CustomCli) -> bool {
    runnable_file(&cli.command)
}

pub fn hook_marker(cli: &CustomCli, end: bool) -> Result<String, String> {
    if cli.id.is_empty()
        || !cli
            .id
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || value == '-')
    {
        return Err("Некорректный идентификатор CLI.".into());
    }
    Ok(format!(
        "<!-- DELEGATOR_CUSTOM_CLI_{} {} -->",
        if end { "END" } else { "START" },
        cli.id
    ))
}

pub fn is_hooked(cli: &CustomCli) -> bool {
    let (Some(path), Ok(start), Ok(end)) = (
        &cli.instructions_path,
        hook_marker(cli, false),
        hook_marker(cli, true),
    ) else {
        return false;
    };
    fs::read_to_string(path)
        .map(|text| text.contains(&start) && text.contains(&end))
        .unwrap_or(false)
}

/// Remove only this CLI's exact managed range. Preserve every other byte.
pub fn update_hook_text(
    existing: &str,
    cli: &CustomCli,
    body: Option<&str>,
) -> Result<String, String> {
    let start = hook_marker(cli, false)?;
    let end = hook_marker(cli, true)?;
    let starts: Vec<_> = existing.match_indices(&start).collect();
    let ends: Vec<_> = existing.match_indices(&end).collect();
    if starts.len() != ends.len() || starts.len() > 1 {
        return Err("Повреждённый блок CLI: файл оставлен без изменений.".into());
    }
    let mut cleaned = existing.to_string();
    if let (Some((begin, _)), Some((finish, _))) = (starts.first(), ends.first()) {
        if finish < begin {
            return Err("Неверный порядок маркеров CLI: файл оставлен без изменений.".into());
        }
        // Our block owns exactly one leading and one trailing LF.
        let begin = if *begin > 0 && existing.as_bytes()[begin - 1] == b'\n' {
            begin - 1
        } else {
            *begin
        };
        let mut finish = finish + end.len();
        if existing.as_bytes().get(finish) == Some(&b'\n') {
            finish += 1;
        }
        cleaned.replace_range(begin..finish, "");
    }
    if let Some(body) = body {
        cleaned.push_str(&format!("\n{start}\n{body}\n{end}\n"));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("delegator-cli-test-{stamp}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn command(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, "This file must never be executed during discovery.").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(self
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("delegator-cli-test-"));
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn entry(command: PathBuf) -> CustomCli {
        CustomCli {
            id: "cli-1".into(),
            name: "My CLI".into(),
            command,
            instructions_path: None,
            enabled: false,
        }
    }
    #[test]
    fn discovers_commands_without_executing_and_deduplicates_path_entries() {
        let temp = Temp::new();
        let filename = if cfg!(target_os = "windows") {
            "my-agent.cmd"
        } else {
            "my-agent"
        };
        let command = temp.command(filename);
        fs::create_dir(temp.0.join("not-a-command.exe")).unwrap();
        let found = discover_in(&[temp.0.clone(), temp.0.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].command, command);
        assert_eq!(
            resolve_command("my-agent", &[temp.0.clone()]),
            Some(command)
        );
        assert!(resolve_command("missing", &[temp.0.clone()]).is_none());
    }
    #[test]
    fn prepare_creates_disabled_entry_and_rejects_duplicates_and_non_markdown_hooks() {
        let temp = Temp::new();
        let path = temp.command(if cfg!(target_os = "windows") {
            "my-agent.exe"
        } else {
            "my-agent"
        });
        let command = path.to_str().unwrap();
        let cli = prepare("My CLI", command, "", &[]).unwrap();
        assert!(!cli.enabled);
        assert!(cli.instructions_path.is_none());
        assert!(prepare("Other", command, "", &[cli]).is_err());
        assert!(prepare(
            "My CLI",
            command,
            temp.0.join("settings.json").to_str().unwrap(),
            &[]
        )
        .is_err());
        assert!(prepare("My CLI", command, "relative.md", &[]).is_err());
    }
    #[test]
    fn missing_executable_is_not_detected() {
        let temp = Temp::new();
        assert!(!is_detected(&entry(temp.0.join("missing.exe"))));
    }
    #[test]
    fn hook_roundtrip_preserves_bom_crlf_and_missing_final_newline() {
        let cli = entry(PathBuf::new());
        for original in ["", "user text", "user text\n", "\u{feff}first\r\nsecond"] {
            let enabled = update_hook_text(original, &cli, Some("Delegator body")).unwrap();
            assert_eq!(
                update_hook_text(&enabled, &cli, Some("Delegator body")).unwrap(),
                enabled
            );
            assert_eq!(update_hook_text(&enabled, &cli, None).unwrap(), original);
            assert_eq!(update_hook_text(original, &cli, None).unwrap(), original);
        }
    }
    #[test]
    fn malformed_or_duplicate_blocks_are_not_rewritten() {
        let cli = entry(PathBuf::new());
        let enabled = update_hook_text("user", &cli, Some("body")).unwrap();
        assert!(update_hook_text(&(enabled.clone() + &enabled), &cli, None).is_err());
        assert!(update_hook_text(&hook_marker(&cli, false).unwrap(), &cli, None).is_err());
        let reverse = format!(
            "{}\n{}",
            hook_marker(&cli, true).unwrap(),
            hook_marker(&cli, false).unwrap()
        );
        assert!(update_hook_text(&reverse, &cli, None).is_err());
    }
    #[test]
    fn another_cli_block_is_preserved() {
        let first = entry(PathBuf::new());
        let second = CustomCli {
            id: "cli-2".into(),
            ..first.clone()
        };
        let one = update_hook_text("user", &first, Some("first")).unwrap();
        let both = update_hook_text(&one, &second, Some("second")).unwrap();
        assert_eq!(update_hook_text(&both, &second, None).unwrap(), one);
    }
    #[cfg(unix)]
    #[test]
    fn unix_discovery_requires_executable_bit() {
        let temp = Temp::new();
        fs::write(temp.0.join("ordinary.txt"), "text").unwrap();
        temp.command("actual-agent");
        assert_eq!(discover_in(&[temp.0.clone()]).len(), 1);
    }
}
