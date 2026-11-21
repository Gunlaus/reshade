use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, env, fs, io::Write, path::{Path, PathBuf}};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub scan_directories: Vec<PathBuf>,
    pub executables: Vec<PathBuf>,
    pub artwork: BTreeMap<String, PathBuf>,
    pub scan_on_launch: bool,
    pub classic: bool,
    pub shared_shaders: bool,
    pub keep_backups: bool,
    pub artwork_limit_mb: u64,
    pub package_limit_mb: u64,
    pub package_directory: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scan_directories: Vec::new(),
            executables: Vec::new(),
            artwork: BTreeMap::new(),
            scan_on_launch: true,
            classic: false,
            shared_shaders: false,
            keep_backups: true,
            artwork_limit_mb: 128,
            package_limit_mb: 2048,
            package_directory: None,
        }
    }
}

pub fn data_directory() -> Result<PathBuf> {
    if let Some(path) = env::var_os("RESHADE_MANAGER_DATA") {
        return Ok(path.into());
    }
    let base = env::var_os("LOCALAPPDATA").or_else(|| env::var_os("XDG_DATA_HOME"))
        .context("The application data directory is unavailable")?;
    Ok(PathBuf::from(base).join("ReShade Manager"))
}

pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("Read {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path.parent().context("The destination has no parent directory")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).with_context(|| format!("Replace {}", path.display()))?;
    Ok(())
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub type Ini = BTreeMap<String, BTreeMap<String, String>>;

pub fn parse_ini(text: &str) -> Ini {
    let mut result = Ini::new();
    let mut section = String::new();
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_ascii_lowercase();
        } else if let Some((key, value)) = line.split_once('=') {
            result.entry(section.clone()).or_default().insert(key.trim().to_ascii_lowercase(), value.trim().into());
        }
    }
    result
}

// Change one key without reserializing unrelated settings, comments or sections.
pub fn set_ini(text: &str, section: &str, key: &str, value: &str) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut next_section = None;
    let mut in_section = section.is_empty();
    let mut found = in_section;
    for (i, line) in lines.iter_mut().enumerate() {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if let Some(name) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if in_section {
                next_section = Some(i);
                break;
            }
            in_section = name.eq_ignore_ascii_case(section);
            found |= in_section;
        } else if in_section && trimmed.split_once('=').is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(key)) {
            *line = format!("{key}={value}");
            return lines.join(newline) + newline;
        }
    }
    if !found {
        if !lines.is_empty() { lines.push(String::new()); }
        lines.push(format!("[{section}]"));
    }
    lines.insert(next_section.unwrap_or(lines.len()), format!("{key}={value}"));
    lines.join(newline) + newline
}

pub fn safe_child(root: &Path, relative: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut path = root.to_owned();
    for component in relative.components() {
        match component {
            Component::Normal(name) if valid_filename(&name.to_string_lossy()) => path.push(name),
            Component::CurDir => continue,
            _ => bail!("Unsafe relative path {}", relative.display()),
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                #[cfg(windows)]
                let reparse = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let reparse = false;
                if metadata.is_symlink() || reparse {
                    bail!("Refusing to modify the linked path {}", path.display());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn valid_filename(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    !name.ends_with(['.', ' ']) && !name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        && !["COM", "LPT"].iter().any(|prefix| stem.strip_prefix(prefix).is_some_and(|s| ["1","2","3","4","5","6","7","8","9","¹","²","³"].contains(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_preserve_existing_config() {
        let original = "\u{feff}# comment\r\n[GENERAL]\r\nPresetPath=Mine.ini\r\n[OTHER]\r\nEnabled=1\r\n";
        let edited = set_ini(original, "GENERAL", "EffectSearchPaths", "Shaders");
        assert!(edited.contains("# comment\r\n"));
        assert_eq!(parse_ini(&edited)["general"]["presetpath"], "Mine.ini");
        assert_eq!(parse_ini(&edited)["other"]["enabled"], "1");
    }

    #[test]
    fn rejects_parent_and_device_paths() {
        assert!(safe_child(Path::new("."), Path::new("../other")).is_err());
        assert!(safe_child(Path::new("."), Path::new("CON.dll")).is_err());
        assert!(safe_child(Path::new("."), Path::new("Shaders/MyEffect.fx")).is_ok());
    }

    #[test]
    fn saves_classic_preference() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        write_json(&path, &Settings { classic: true, ..Settings::default() }).unwrap();
        assert!(read_json::<Settings>(&path).unwrap().classic);
    }
}
