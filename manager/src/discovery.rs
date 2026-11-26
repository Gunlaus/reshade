use crate::{pe, storage::Settings};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, HashMap}, fs, path::{Path, PathBuf}};
use walkdir::WalkDir;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Source { Steam, Epic, Gog, Custom, Manual }

impl Source {
    pub fn label(&self) -> &'static str {
        match self { Self::Steam => "Steam", Self::Epic => "Epic Games", Self::Gog => "GOG Galaxy", Self::Custom => "Custom directory", Self::Manual => "Manual" }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Game {
    pub title: String,
    pub executable: PathBuf,
    pub source: Source,
    pub store_id: Option<String>,
    pub artwork_paths: Vec<PathBuf>,
}

impl Game {
    pub fn key(&self) -> String {
        let path = self.executable.to_string_lossy();
        if cfg!(windows) { path.to_lowercase() } else { path.into_owned() }
    }
}

#[derive(Default)]
pub struct Locations {
    pub steam: Vec<PathBuf>,
    pub epic: Vec<PathBuf>,
    pub gog: Vec<GogGame>,
}

pub struct GogGame {
    pub id: String,
    pub title: String,
    pub directory: PathBuf,
    pub executable: Option<PathBuf>,
    pub art: Vec<PathBuf>,
}

pub fn discover(locations: &Locations, settings: &Settings) -> Vec<Game> {
    let mut result = BTreeMap::new();
    let mut record = |title: String, executable: PathBuf, source, store_id, artwork_paths| {
        if let Ok(executable) = executable.canonicalize() {
            let game = Game { title, executable, source, store_id, artwork_paths };
            result.entry(game.key()).or_insert(game);
        }
    };
    for root in &locations.steam {
        for library in steam_libraries(root) {
            let steamapps = library.join("steamapps");
            for path in entries(&steamapps) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if !name.starts_with("appmanifest_") || !name.ends_with(".acf") { continue; }
                let kv = vdf_strings(&fs::read_to_string(&path).unwrap_or_default());
                if let (Some(id), Some(title), Some(dir)) = (kv.get("appid"), kv.get("name"), kv.get("installdir")) {
                    if !id.bytes().all(|b| b.is_ascii_digit()) { continue; }
                    let Ok(directory) = crate::storage::safe_child(&steamapps.join("common"), Path::new(dir)) else { continue };
                    if let Some(exe) = choose_exe(&directory, title) {
                        let cache = root.join("appcache/librarycache");
                        let mut art: Vec<_> = ["library_600x900.jpg", "library_600x900_2x.jpg", "header.jpg", "library_hero.jpg"].iter().flat_map(|name| {
                            [cache.join(id).join(name), cache.join(format!("{id}_{name}"))]
                        }).collect();
                        art.extend(WalkDir::new(cache.join(id)).max_depth(3).into_iter().filter_map(Result::ok)
                            .filter(|item| matches!(item.file_name().to_str(), Some("library_600x900.jpg" | "library_600x900_2x.jpg" | "header.jpg")))
                            .map(|item| item.into_path()));
                        record(title.into(), exe, Source::Steam, Some(id.into()), art);
                    }
                }
            }
        }
    }
    for manifest_dir in &locations.epic {
        for path in entries(manifest_dir).filter(|p| p.extension().is_some_and(|ext| ext == "item")) {
            let Some(item) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<EpicItem>(&bytes).ok()) else { continue };
            if item.incomplete || item.is_dlc || !Path::new(&item.directory).is_absolute() { continue; }
            let dir = PathBuf::from(&item.directory);
            let exe = crate::storage::safe_child(&dir, Path::new(&item.launch)).ok().filter(|p| executable(p))
                .or_else(|| choose_exe(&dir, &item.title));
            if let Some(exe) = exe {
                let art = item.artwork.into_iter().map(PathBuf::from).filter(|p| p.is_absolute()).collect();
                record(item.title, exe, Source::Epic, Some(item.id), art);
            }
        }
    }
    for item in &locations.gog {
        if let Some(exe) = item.executable.clone().filter(|p| executable(p)).or_else(|| choose_exe(&item.directory, &item.title)) {
            record(item.title.clone(), exe, Source::Gog, Some(item.id.clone()), item.art.clone());
        }
    }
    for dir in &settings.scan_directories {
        for entry in game_executables(dir, 5).take(2000) {
            record(title_for(&entry), entry, Source::Custom, None, Vec::new());
        }
    }
    for exe in settings.executables.iter().filter(|path| executable(path)) {
        record(title_for(exe), exe.clone(), Source::Manual, None, Vec::new());
    }
    for exe in settings.executables.iter().filter(|path| !path.is_file()) {
        let game = Game { title: title_for(exe), executable: exe.clone(), source: Source::Manual,
            store_id: None, artwork_paths: Vec::new() };
        result.entry(game.key()).or_insert(game);
    }
    let mut games: Vec<_> = result.into_values().collect();
    games.sort_by_cached_key(|g| (g.title.to_lowercase(), g.key()));
    games
}

fn title_for(path: &Path) -> String {
    pe::version_string(path, "FileDescription").filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned())
}

#[derive(Deserialize)]
struct EpicItem {
    #[serde(rename = "DisplayName")]
    title: String,
    #[serde(rename = "InstallLocation")]
    directory: String,
    #[serde(rename = "LaunchExecutable", default)]
    launch: String,
    #[serde(rename = "CatalogItemId", default)]
    id: String,
    #[serde(rename = "bIsIncompleteInstall", default)]
    incomplete: bool,
    #[serde(rename = "bIsDLC", default)]
    is_dlc: bool,
    #[serde(rename = "ArtworkPaths", default)]
    artwork: Vec<String>,
}

fn entries(path: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(path).into_iter().flatten().filter_map(Result::ok).map(|e| e.path())
}

fn steam_libraries(root: &Path) -> Vec<PathBuf> {
    let mut result = vec![root.to_owned()];
    for location in ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"] {
        let Ok(text) = fs::read_to_string(root.join(location)) else { continue };
        for (key, value) in vdf_pairs(&text) {
            let path = PathBuf::from(value);
            if (key == "path" || key.bytes().all(|b| b.is_ascii_digit())) && path.is_absolute() && !result.contains(&path) {
                result.push(path);
            }
        }
    }
    result
}

fn vdf_strings(input: &str) -> HashMap<String, String> {
    vdf_pairs(input).into_iter().collect()
}

fn vdf_pairs(input: &str) -> Vec<(String, String)> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' | '}' => tokens.push(None),
            '/' if chars.peek() == Some(&'/') => { for c in chars.by_ref() { if c == '\n' { break; } } }
            '"' => {
                let mut value = String::new();
                while let Some(c) = chars.next() {
                    if c == '"' { break; }
                    if c == '\\' && chars.peek().is_some_and(|next| matches!(next, '\\' | '"')) {
                        value.push(chars.next().unwrap_or_default());
                    } else { value.push(c); }
                }
                tokens.push(Some(value));
            }
            _ => {}
        }
    }
    let mut keys = Vec::new();
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        if let (Some(key), Some(Some(_))) = (token, tokens.peek()) {
            if let Some(Some(value)) = tokens.next() { keys.push((key.to_ascii_lowercase(), value)); }
        }
    }
    keys
}

pub fn executable(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) && pe::architecture(path).is_ok()
}

fn game_executables(root: &Path, depth: usize) -> impl Iterator<Item = PathBuf> + '_ {
    WalkDir::new(root).max_depth(depth).follow_links(false).follow_root_links(false).into_iter().filter_entry(|item| {
        if item.depth() == 0 { return true; }
        let name = item.file_name().to_string_lossy().to_ascii_lowercase();
        !matches!(name.as_str(), "redist" | "_commonredist" | "prerequisites" | "easyanticheat" | "battleye" | "engine" | "installer" | "crashreportclient")
    }).take(25_000).filter_map(Result::ok).filter(move |item| item.file_type().is_file() && {
        let stem = item.path().file_stem().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        !stem.starts_with("unins") && !["setup", "crashreport", "unitycrashhandler", "vc_redist", "dxsetup", "vcredist", "benchmark"].iter().any(|part| stem.contains(part))
    }).map(|item| item.into_path()).filter(|p| executable(p))
}

fn choose_exe(root: &Path, title: &str) -> Option<PathBuf> {
    fn compact(text: &str) -> String { text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect() }
    let title = compact(title);
    game_executables(root, 6).take(256).max_by_key(|path| {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        let score = if name.ends_with("-shipping") { 4 } else if compact(&name) == title { 3 } else if name.contains("launcher") { 0 } else { 2 };
        (score, std::cmp::Reverse(path.components().count()), fs::metadata(path).map(|m| m.len()).unwrap_or(0), std::cmp::Reverse(path.clone()))
    })
}

#[cfg(windows)]
pub fn system_locations() -> Locations {
    use winreg::{enums::*, RegKey};
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut locations = Locations::default();
    for (hive, key, value) in [(&hkcu, r"Software\Valve\Steam", "SteamPath"), (&hklm, r"Software\Wow6432Node\Valve\Steam", "InstallPath"), (&hklm, r"Software\Valve\Steam", "InstallPath")] {
        if let Ok(path) = hive.open_subkey(key).and_then(|k| k.get_value::<String, _>(value)) {
            let path = PathBuf::from(path);
            if !locations.steam.contains(&path) { locations.steam.push(path); }
        }
    }
    if let Some(base) = std::env::var_os("ProgramData") {
        locations.epic.push(PathBuf::from(base).join("Epic/EpicGamesLauncher/Data/Manifests"));
    }
    for key in [r"Software\Wow6432Node\GOG.com\Games", r"Software\GOG.com\Games"] {
        let Ok(games) = hklm.open_subkey(key) else { continue };
        for id in games.enum_keys().filter_map(Result::ok) {
            let Ok(game) = games.open_subkey(&id) else { continue };
            let Ok(directory) = game.get_value::<String, _>("path").map(PathBuf::from) else { continue };
            let title = game.get_value("gameName").unwrap_or_else(|_| directory.file_name().unwrap_or_default().to_string_lossy().into_owned());
            let executable = game.get_value::<String,_>("exe").ok().map(|exe| directory.join(exe));
            let art = std::env::var_os("ProgramData").into_iter().flat_map(|root| {
                let cache = PathBuf::from(root).join("GOG.com/Galaxy/webcache");
                [cache.join(format!("{id}_verticalCover.jpg")), cache.join(format!("{id}_header.jpg"))]
            }).collect();
            locations.gog.push(GogGame { id, title, directory, executable, art });
        }
    }
    locations
}

#[cfg(not(windows))]
pub fn system_locations() -> Locations { Locations::default() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_launcher_fixtures_and_deduplicates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let steam = root.join("Steam");
        let library = root.join("Library");
        let game = library.join("steamapps/common/Test");
        let epic = root.join("Epic");
        let gog = root.join("Gog");
        for path in [steam.join("steamapps"), game.clone(), epic.clone(), gog.clone()] { fs::create_dir_all(path).unwrap(); }
        fs::write(steam.join("steamapps/libraryfolders.vdf"), format!("\"libraryfolders\" {{ \"2\" {{ \"path\" \"{}\" }} }}", library.display().to_string().replace('\\', "\\\\"))).unwrap();
        fs::write(library.join("steamapps/appmanifest_10.acf"), "\"AppState\" { \"appid\" \"10\" \"name\" \"Test\" \"installdir\" \"Test\" }").unwrap();
        let fixture = std::env::current_exe().unwrap();
        fs::copy(&fixture, game.join("Test.exe")).unwrap();
        fs::copy(&fixture, gog.join("Gog.exe")).unwrap();
        let epic_game = epic.join("Epic.exe");
        fs::copy(&fixture, &epic_game).unwrap();
        fs::write(epic.join("one.item"), serde_json::to_vec(&serde_json::json!({"DisplayName":"Epic","InstallLocation":epic,"LaunchExecutable":"Epic.exe","CatalogItemId":"epic-id"})).unwrap()).unwrap();
        let locations = Locations { steam: vec![steam], epic: vec![epic.clone()], gog: vec![GogGame { id: "123".into(), title: "GOG".into(), directory: gog, executable: None, art: vec![] }] };
        let settings = Settings { scan_directories: vec![epic], executables: vec![game.join("Test.exe")], ..Settings::default() };
        #[cfg(windows)]
        assert_eq!(discover(&locations, &settings).iter().map(|g| g.source.clone()).collect::<Vec<_>>(), [Source::Epic, Source::Gog, Source::Steam]);
        #[cfg(not(windows))]
        assert!(discover(&locations, &settings).is_empty());
    }
}
