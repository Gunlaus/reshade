use crate::{catalog::{self, ArchiveFile, Cache, Package, Selection}, pe::{self, Api, Arch, Target}, storage::{atomic_write, parse_ini, read_json, safe_child, set_ini, sha256, write_json}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs, path::{Path, PathBuf}};

pub const RUNTIME_VERSION: &str = "6.8.0";
const STANDARD_SETUP_SHA256: &str = "207aea16205fbf952bc8fe1879966672454cf04002e7ad34237c7990a5b3c0b4";
const ADDON_SETUP_SHA256: &str = "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445";

const RECEIPT: &str = ".reshade-manager/installation.json";
const JOURNAL: &str = ".reshade-manager/transaction.json";
const WRAPPERS: [&str; 7] = ["d3d9.dll", "dxgi.dll", "d3d10.dll", "d3d11.dll", "d3d12.dll", "opengl32.dll", "ddraw.dll"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation { Install, Update, Repair, Modify, Uninstall }

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Receipt {
    pub game: Option<PathBuf>,
    pub runtime: Option<String>,
    pub api: Option<Api>,
    pub addon: bool,
    pub selection: Selection,
    pub selection_managed: bool,
    owned: BTreeMap<PathBuf, String>,
    live_effects: BTreeSet<String>,
    live_textures: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub wrapper: Option<PathBuf>,
    pub version: Option<String>,
    pub conflict: bool,
    pub configuration: bool,
    pub vulkan: bool,
}

pub fn status(executable: &Path, base: &Path) -> Status {
    let mut status = Status { configuration: base.join("ReShade.ini").is_file(), ..Status::default() };
    for name in WRAPPERS {
        let path = base.join(name);
        let product = pe::version_string(&path, "ProductName");
        if product.as_deref() == Some("ReShade") {
            if status.wrapper.is_some() { status.conflict = true; }
            status.version = pe::version_string(&path, "ProductVersion");
            status.wrapper = Some(path);
        }
    }
    if let Some(data) = std::env::var_os("ProgramData") {
        let global = PathBuf::from(data).join("ReShade");
        let installed = fs::read_to_string(global.join("ReShadeApps.ini")).ok()
            .is_some_and(|text| parse_ini(&text).get("").and_then(|values| values.get("apps"))
                .is_some_and(|paths| paths.split(',').any(|path| same_path(Path::new(path.trim()), executable))));
        if installed {
            status.vulkan = true;
            let number = pe::architecture(executable).map(Arch::number).unwrap_or(64);
            status.version = pe::version_string(&global.join(format!("ReShade{number}.dll")), "ProductVersion").or(status.version);
        }
    }
    status
}

fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.to_string_lossy();
    let b = b.to_string_lossy();
    a.trim_start_matches(r"\\?\").eq_ignore_ascii_case(b.trim_start_matches(r"\\?\"))
}

pub fn receipt(base: &Path) -> Result<Receipt> { read_json(&safe_child(base, Path::new(RECEIPT))?) }

pub fn adopt(target: &Target, addon: bool) -> Result<()> {
    let base = &target.base;
    let root = target.executable.parent().context("The executable has no parent directory")?;
    let _lock = lock(root)?;
    recover(root)?;
    if receipt(base)?.game.is_some() { bail!("This directory already has an installation record."); }
    let status = status(&target.executable, base);
    if status.conflict || status.vulkan { bail!("Open legacy setup to manage this shared layer or wrapper conflict."); }
    let wrapper = status.wrapper.context("No ReShade runtime was found.")?;
    let api = Api::ALL.into_iter().find(|api| wrapper.file_stem().is_some_and(|name| name.eq_ignore_ascii_case(api.name())));
    let imported = Receipt { game: Some(target.executable.clone()), runtime: status.version, api, addon, ..Receipt::default() };
    write_json(&safe_child(base, Path::new(RECEIPT))?, &imported)
}

pub fn runtime_url(addon: bool) -> String {
    let mode = if addon { "_Addon" } else { "" };
    format!("https://reshade.me/downloads/ReShade_Setup_{RUNTIME_VERSION}{mode}.exe")
}

pub fn verify_distribution(bytes: &[u8], addon: bool) -> Result<()> {
    let expected = if addon { ADDON_SETUP_SHA256 } else { STANDARD_SETUP_SHA256 };
    if sha256(bytes) != expected {
        bail!("This file does not match the pinned official ReShade {RUNTIME_VERSION} distribution.");
    }
    Ok(())
}

fn distribution_dll(bytes: &[u8], arch: Arch, addon: bool) -> Result<Vec<u8>> {
    #[cfg(not(test))]
    verify_distribution(bytes, addon)?;
    #[cfg(test)]
    let _ = addon;
    extract_runtime(bytes, arch)
}

pub fn wrapper_name(api: Api) -> Result<String> {
    if api == Api::Vulkan { bail!("Vulkan uses the legacy setup to manage the shared Windows layer."); }
    Ok(format!("{}.dll", api.name()))
}

pub struct Plan<'a> {
    pub target: &'a Target,
    pub api: Api,
    pub addon: bool,
    pub acknowledged: bool,
    pub operation: Operation,
    pub selection: &'a Selection,
    pub shaders: &'a [Package],
    pub addons: &'a [Package],
    pub shared: Option<&'a Path>,
    pub backups: Option<&'a Path>,
}

pub fn execute(cache: &Cache, plan: Plan<'_>, mut progress: impl FnMut(&str)) -> Result<()> {
    let target = plan.target;
    let base = &target.base;
    if target.banned && plan.operation != Operation::Uninstall { bail!("Upstream compatibility metadata marks this executable as blocked."); }
    if plan.operation == Operation::Modify && target.base != target.executable.parent().context("Executable folder unavailable")? && !plan.selection.addons.is_empty() {
        bail!("Use legacy setup to modify add-ons for a redirected install directory.");
    }
    if plan.addon && !plan.acknowledged && plan.operation != Operation::Uninstall {
        bail!("Acknowledge the add-on support warning before continuing.");
    }
    if target.forced && target.api != Some(plan.api) { bail!("This executable requires its upstream compatibility API."); }
    let module = PathBuf::from(wrapper_name(plan.api)?);
    if !base.is_dir() { bail!("The destination directory does not exist."); }
    let root = target.executable.parent().context("The executable has no parent directory")?;
    let prefix = base.strip_prefix(root).context("The install destination is outside the executable directory")?;
    let _lock = lock(root)?;
    recover(root)?;
    let mut previous = receipt(base)?;
    if previous.game.as_ref().is_some_and(|exe| !same_path(exe, &target.executable)) {
        bail!("This directory already contains settings managed for another executable. Select that game to manage its installation.");
    }
    let existing = status(&target.executable, base);
    if plan.operation == Operation::Update && existing.version.as_deref().is_some_and(is_newer_than_distribution) {
        bail!("The installed runtime is newer than the manager’s pinned distribution.");
    }
    if existing.conflict || existing.vulkan { bail!("Multiple runtime wrappers or a shared Vulkan layer are present. Resolve the existing installation in legacy setup."); }
    let destination = safe_child(base, &module)?;
    if destination.exists() && pe::version_string(&destination, "ProductName").as_deref() != Some("ReShade") {
        bail!("{} is occupied by another graphics wrapper. Move it manually before installing.", destination.display());
    }
    if plan.operation == Operation::Uninstall {
        progress("Preparing uninstall");
        let mut changes = BTreeMap::new();
        if let Some(wrapper) = existing.wrapper {
            changes.insert(PathBuf::from(wrapper.file_name().context("Missing wrapper filename")?), None);
        }
        for (file, checksum) in &previous.owned {
            if !is_managed_resource(file) { continue; }
            let path = safe_child(base, file)?;
            if fs::read(path).is_ok_and(|bytes| sha256(&bytes) == *checksum) {
                changes.insert(file.clone(), None);
            }
        }
        let config_path = safe_child(base, Path::new("ReShade.ini"))?;
        if let Ok(text) = fs::read_to_string(&config_path) {
            let text = search_path(&text, "EffectSearchPaths", &previous.live_effects, &BTreeSet::new())?;
            let text = search_path(&text, "TextureSearchPaths", &previous.live_textures, &BTreeSet::new())?;
            changes.insert("ReShade.ini".into(), Some(text.into_bytes()));
        }
        changes.insert(RECEIPT.into(), None);
        progress("Removing ReShade runtime and verified managed files");
        return apply(root, relocate(changes, prefix), plan.backups);
    }

    let client = catalog::client()?;
    let mut changes = BTreeMap::new();
    if plan.operation != Operation::Modify {
        progress("Downloading or reusing the ReShade distribution");
        let url = runtime_url(plan.addon);
        let runtime = |data: &[u8]| distribution_dll(data, target.arch, plan.addon);
        let (_, data) = cache.obtain(&client, &url, false, false, |d| runtime(d).map(|_| ()))?;
        progress("Validating the runtime and executable architecture");
        let dll = runtime(&data)?;
        changes.insert(module.clone(), Some(dll));
        if let Some(old) = existing.wrapper {
            let old_name = PathBuf::from(old.file_name().context("Missing wrapper filename")?);
            if old_name != module {
                previous.owned.remove(&old_name);
                changes.insert(old_name, None);
            }
        }
        previous.runtime = Some(RUNTIME_VERSION.into());
        previous.api = Some(plan.api);
        previous.addon = plan.addon;
    }
    let mut effects = BTreeSet::new();
    let mut effect_names = BTreeSet::new();
    let mut textures = BTreeSet::new();
    let manage_content = matches!(plan.operation, Operation::Install | Operation::Modify)
        || (plan.operation == Operation::Repair && previous.selection_managed);
    if manage_content {
        let mut owned = BTreeMap::new();
        for package in plan.shaders {
            let Some(choice) = plan.selection.shaders.get(&package.id) else { continue; };
            progress(&format!("Downloading or validating {}", package.name));
            let (cached, bytes) = cache.obtain(&client, &package.url, true, plan.operation == Operation::Modify,
                |data| catalog::files(data, shader_file).map(|_| ()))?;
            progress(&format!("Extracting {}", package.name));
            let entries = shader_entries(catalog::files(&bytes, shader_file)?, package, choice)?;
            for entry in &entries {
                if entry.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("fx")) {
                    let name = entry.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                    if !effect_names.insert(name.clone()) {
                        bail!("More than one selected shader package provides {name}. Adjust the catalog selection.");
                    }
                }
            }
            let private = PathBuf::from(".reshade-manager/library").join(sha256(package.url.as_bytes()));
            if let Some(shared) = plan.shared {
                let key = sha256(serde_json::to_vec(&(cached.digest, &package.denied, choice))?.as_slice());
                let folder = shared.join(key);
                publish_shared(&folder, &entries)?;
                if entries.iter().any(|e| e.path.starts_with("Shaders")) { effects.insert(path_setting(&folder.join("Shaders"), true)?); }
                if entries.iter().any(|e| e.path.starts_with("Textures")) { textures.insert(path_setting(&folder.join("Textures"), true)?); }
            } else {
                if entries.iter().any(|e| e.path.starts_with("Shaders")) { effects.insert(path_setting(&Path::new(".").join(&private).join("Shaders"), true)?); }
                if entries.iter().any(|e| e.path.starts_with("Textures")) { textures.insert(path_setting(&Path::new(".").join(&private).join("Textures"), true)?); }
                for item in entries {
                    stage_owned(base, &previous, &mut owned, &mut changes, private.join(item.path), item.bytes)?;
                }
            }
        }
        if plan.addon {
            if base != root && !plan.selection.addons.is_empty() { bail!("Use legacy setup for add-ons in redirected installation directories."); }
            let config = optional_config(&safe_child(root, Path::new("ReShade.ini"))?)?;
            let ini = parse_ini(&config);
            let addon_dir = ini.get("addon").and_then(|v| v.get("addonpath")).filter(|p| !p.trim().is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            if !plan.selection.addons.is_empty() { safe_child(base, &addon_dir)?; }
            for package in plan.addons.iter().filter(|p| plan.selection.addons.contains(&p.id)) {
                progress(&format!("Downloading or validating {}", package.name));
                let (_, bytes) = cache.obtain(&client, &package.url, false, false, |data| addon_entries(package, data, target.arch).map(|_| ()))?;
                for item in addon_entries(package, &bytes, target.arch)? {
                    let destination = if item.path.starts_with(".reshade-manager") { item.path } else { addon_dir.join(item.path) };
                    stage_owned(base, &previous, &mut owned, &mut changes, destination, item.bytes)?;
                }
            }
        }
        for (file, checksum) in &previous.owned {
            if owned.contains_key(file) || !is_managed_resource(file) { continue; }
            let path = safe_child(base, file)?;
            if fs::read(path).is_ok_and(|data| sha256(&data) == *checksum) {
                changes.insert(file.clone(), None);
            }
        }
        previous.owned.retain(|file, _| file.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")));
        previous.owned.extend(owned);
        previous.selection = plan.selection.clone();
        previous.selection_managed = true;
    } else {
        effects = previous.live_effects.clone();
        textures = previous.live_textures.clone();
    }

    progress("Configuring effect and texture search paths");
    let config_path = safe_child(base, Path::new("ReShade.ini"))?;
    let original = optional_config(&config_path)?;
    if !effect_names.is_empty() {
        check_existing_effects(base, &original, &previous.live_effects, &effect_names)?;
    }
    let config = if plan.operation == Operation::Update && !original.is_empty() {
        original
    } else {
        configure(&original, target, &previous, &effects, &textures)?
    };
    changes.insert("ReShade.ini".into(), Some(config.into_bytes()));
    let mut changes = relocate(changes, prefix);
    if !prefix.as_os_str().is_empty() {
        let global = optional_config(&safe_child(root, Path::new("ReShade.ini"))?)?;
        let base_path = path_setting(base, false)?;
        let global = set_ini(&global, "INSTALL", "BasePath", &base_path);
        changes.insert("ReShade.ini".into(), Some(global.into_bytes()));
    }
    previous.game = Some(target.executable.clone());
    previous.live_effects = effects;
    previous.live_textures = textures;
    changes.insert(prefix.join(RECEIPT), Some(serde_json::to_vec_pretty(&previous)?));
    progress("Installing and verifying files");
    apply(root, changes, plan.backups)?;
    progress("Final verification complete");
    Ok(())
}

fn is_newer_than_distribution(installed: &str) -> bool {
    fn version(input: &str) -> Vec<u32> {
        let mut parts: Vec<_> = input.trim_start_matches('v').split('.').take(3)
            .map(|part| part.parse::<u32>().unwrap_or(0)).collect();
        parts.resize(3, 0);
        parts
    }
    version(installed) > version(RUNTIME_VERSION)
}

fn relocate(changes: BTreeMap<PathBuf, Option<Vec<u8>>>, prefix: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    changes.into_iter().map(|(path, change)| (prefix.join(path), change)).collect()
}

fn optional_config(config: &Path) -> Result<String> {
    match fs::read_to_string(config) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("Read {}", config.display())),
    }
}

fn path_setting(path: &Path, recursive: bool) -> Result<String> {
    let text = path.to_str().context("ReShade search paths require Unicode paths")?;
    let text = text.trim_start_matches(r"\\?\").replace('/', "\\");
    if text.contains([',', '\n', '\r']) { bail!("ReShade cannot represent this search path."); }
    Ok(if recursive { format!("{text}\\**") } else { text })
}

fn search_path(text: &str, key: &str, old: &BTreeSet<String>, new: &BTreeSet<String>) -> Result<String> {
    let ini = parse_ini(text);
    let raw = ini.get("general").and_then(|s| s.get(&key.to_ascii_lowercase()));
    if raw.is_none() && new.is_empty() { return Ok(text.to_owned()); }
    let mut paths = raw.map(String::as_str).unwrap_or(if key == "EffectSearchPaths" { r".\reshade-shaders\Shaders\**" } else { r".\reshade-shaders\Textures\**" })
        .split(',').map(str::trim).filter(|entry| !entry.is_empty() && !old.contains(*entry)).map(str::to_owned).collect::<Vec<_>>();
    for path in new {
        if !paths.contains(path) { paths.push(path.clone()); }
    }
    if raw.is_some_and(|current| *current == paths.join(",")) { return Ok(text.to_owned()); }
    Ok(set_ini(text, "GENERAL", key, &paths.join(",")))
}

fn check_existing_effects(base: &Path, config: &str, old_paths: &BTreeSet<String>, desired: &BTreeSet<String>) -> Result<()> {
    let ini = parse_ini(config);
    let paths = ini.get("general").and_then(|s| s.get("effectsearchpaths")).map(String::as_str)
        .unwrap_or(r".\reshade-shaders\Shaders\**");
    for path in paths.split(',').map(str::trim).filter(|p| !p.is_empty() && !old_paths.contains(*p)) {
        let (path, recursive) = path.strip_suffix(r"\**").or_else(|| path.strip_suffix("/**")).map_or((path, false), |p| (p, true));
        let path = base.join(path);
        if !path.is_dir() { continue; }
        for entry in walkdir::WalkDir::new(path).follow_links(false).max_depth(if recursive { 16 } else { 1 })
            .into_iter().take(20_000).filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if entry.file_type().is_file() && desired.contains(&name) {
                bail!("A selected effect already exists at {}. Deselect that effect to keep using this copy.", entry.path().display());
            }
        }
    }
    Ok(())
}

fn configure(original: &str, target: &Target, previous: &Receipt, effects: &BTreeSet<String>, textures: &BTreeSet<String>) -> Result<String> {
    let mut text = search_path(original, "EffectSearchPaths", &previous.live_effects, effects)?;
    text = search_path(&text, "TextureSearchPaths", &previous.live_textures, textures)?;
    let original_ini = parse_ini(original);
    if !original_ini.get("general").is_some_and(|section| section.contains_key("preprocessordefinitions")) {
        let value = |key| target.compatibility.get(key).map(String::as_str).unwrap_or("0");
        let definitions = format!("RESHADE_DEPTH_LINEARIZATION_FAR_PLANE=1000.0,RESHADE_DEPTH_INPUT_IS_UPSIDE_DOWN={},RESHADE_DEPTH_INPUT_IS_REVERSED={},RESHADE_DEPTH_INPUT_IS_LOGARITHMIC={}", value("depthupsidedown"), value("depthreversed"), value("depthlogarithmic"));
        text = set_ini(&text, "GENERAL", "PreprocessorDefinitions", &definitions);
    }
    for name in ["DepthCopyBeforeClears", "DepthCopyAtClearIndex", "DrawStatsHeuristic", "UseAspectRatioHeuristics"] {
        let key = name.to_ascii_lowercase();
        if let Some(value) = target.compatibility.get(&key).filter(|_| !original_ini.get("depth").is_some_and(|s| s.contains_key(&key))) {
            text = set_ini(&text, "DEPTH", name, value);
        }
    }
    Ok(text)
}

fn extract_runtime(data: &[u8], arch: Arch) -> Result<Vec<u8>> {
    if !data.starts_with(b"MZ") { bail!("The distribution is not an official setup executable."); }
    let x86 = "ReShade32.dll";
    let x64 = "ReShade64.dll";
    let files = catalog::files(data, |n| n == x86 || n == x64)
        .or_else(|_| {
            for (offset, block) in data.chunks(512).enumerate() {
                if block.starts_with(b"PK\x03\x04") {
                    if let Ok(files) = catalog::files(&data[offset * 512..], |n| n == x86 || n == x64) {
                        if files.len() == 2 { return Ok(files); }
                    }
                }
            }
            bail!("Unable to read the ReShade setup archive")
        })?;
    if files.len() != 2 { bail!("The setup distribution does not contain both ReShade architectures."); }
    let mut selected = None;
    for entry in files {
        let expected = if entry.path == Path::new(x86) { Arch::X86 } else { Arch::X64 };
        validate_dll(&entry.bytes, expected, true)?;
        if expected == arch { selected = Some(entry.bytes); }
    }
    selected.context("The selected ReShade architecture is missing")
}

fn validate_dll(bytes: &[u8], arch: Arch, reshade: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let dll = temp.path().join("candidate.dll");
    fs::write(&dll, bytes)?;
    if pe::architecture(&dll)? != arch { bail!("The downloaded binary has the wrong executable architecture."); }
    let pe = goblin::pe::PE::parse(bytes)?;
    if !pe.is_lib { bail!("The downloaded file is not a DLL."); }
    if reshade && pe::version_string(&dll, "ProductName").as_deref() != Some("ReShade") { bail!("The runtime product metadata did not identify ReShade."); }
    Ok(())
}

fn shader_file(name: &str) -> bool {
    let extension = Path::new(name).extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    matches!(extension.as_str(), "fx" | "fxh" | "png" | "jpg" | "jpeg" | "bmp" | "dds" | "tga" | "webp" | "tif" | "tiff")
        || is_license(Path::new(name))
}

fn is_license(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    name.starts_with("license") || name.starts_with("copying") || name == "notice" || name == "notice.txt"
}

fn shader_entries(files: Vec<ArchiveFile>, package: &Package, choice: &catalog::Choice) -> Result<Vec<ArchiveFile>> {
    let mut output = Vec::new();
    let texture_extensions = ["png", "jpg", "jpeg", "bmp", "dds", "tga", "webp", "tif", "tiff"];
    let effect_root = find_root(&files, "Shaders", &["fx"]);
    let texture_root = find_root(&files, "Textures", &texture_extensions);
    let desired: BTreeSet<_> = choice.effects.iter().map(|name| name.to_ascii_lowercase()).collect();
    let mut found = BTreeSet::new();
    for file in files {
        let path = &file.path;
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        let extension = path.extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        let mapped = if is_license(path) {
            Some(PathBuf::from("Licenses").join(path.components().skip(1).collect::<PathBuf>()))
        } else if texture_extensions.contains(&extension.as_str()) {
            texture_root.as_ref().and_then(|root| path.strip_prefix(root).ok()).map(|p| Path::new("Textures").join(p))
        } else if extension == "fx" || extension == "fxh" {
            if extension == "fx" && (package.denied.iter().any(|e| e.eq_ignore_ascii_case(&name)) || (!choice.all && !desired.contains(&name))) { continue; }
            effect_root.as_ref().and_then(|root| path.strip_prefix(root).ok()).map(|p| {
                if extension == "fx" { found.insert(name); }
                Path::new("Shaders").join(p)
            })
        } else { None };
        if let Some(path) = mapped {
            safe_child(Path::new("."), &path)?;
            output.push(ArchiveFile { path, bytes: file.bytes });
        }
    }
    if found.is_empty() || (!choice.all && !desired.is_subset(&found)) {
        bail!("{} does not contain all selected effects in its supported shader layout.", package.name);
    }
    Ok(output)
}

fn find_root(files: &[ArchiveFile], preferred: &str, extensions: &[&str]) -> Option<PathBuf> {
    files.iter().filter(|entry| entry.path.extension().is_some_and(|ext| extensions.iter().any(|e| ext.eq_ignore_ascii_case(e))))
        .filter_map(|entry| {
            let parent = entry.path.parent()?;
            let mut search = parent.ancestors();
            search.find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(preferred))).map(|p| (false, p))
                .or_else(|| Some((true, parent)))
        }).min_by_key(|(fallback, path)| (*fallback, path.components().count())).map(|(_, path)| path.to_owned())
}

fn addon_entries(package: &Package, bytes: &[u8], arch: Arch) -> Result<Vec<ArchiveFile>> {
    let url = reqwest::Url::parse(&package.url)?;
    let filename = url.path_segments().and_then(Iterator::last).context("Add-on filename is missing")?;
    let ext = format!("addon{}", arch.number());
    let recognized = |path: &Path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("addon") || e.eq_ignore_ascii_case(ext.as_str()));
    let mut files = if recognized(Path::new(filename)) {
        vec![ArchiveFile { path: filename.into(), bytes: bytes.to_vec() }]
    } else {
        catalog::files(bytes, |n| recognized(Path::new(n)) || is_license(Path::new(n)))?
    };
    let matching: Vec<_> = files.iter().enumerate().filter(|(_, f)| recognized(&f.path))
        .filter(|(_, f)| validate_dll(&f.bytes, arch, false).is_ok()).map(|(i, _)| i).collect();
    let [index] = matching.as_slice() else {
        bail!("{} has no unique binary for {}.", package.name, arch.label());
    };
    let mut module = files.swap_remove(*index);
    let name = module.path.file_stem().context("Missing add-on name")?;
    module.path = PathBuf::from(name).with_extension(&ext);
    let license_dir = Path::new(".reshade-manager/licenses").join(sha256(package.url.as_bytes()));
    let mut result = vec![module];
    for file in files.into_iter().filter(|f| is_license(&f.path)) {
        result.push(ArchiveFile { path: license_dir.join(file.path), bytes: file.bytes });
    }
    Ok(result)
}

fn is_managed_resource(path: &Path) -> bool {
    path.starts_with(".reshade-manager/library") || path.starts_with(".reshade-manager/licenses")
        || path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("addon32") || ext.eq_ignore_ascii_case("addon64"))
}

fn stage_owned(base: &Path, previous: &Receipt, owned: &mut BTreeMap<PathBuf, String>, changes: &mut BTreeMap<PathBuf, Option<Vec<u8>>>, relative: PathBuf, bytes: Vec<u8>) -> Result<()> {
    if !is_managed_resource(&relative) { bail!("Invalid managed resource path."); }
    let file = safe_child(base, &relative)?;
    let checksum = sha256(&bytes);
    if owned.keys().any(|path| same_path(path, &relative)) { bail!("Two packages claim {}", file.display()); }
    if file.exists() {
        let current = sha256(&fs::read(&file)?);
        if previous.owned.get(&relative) != Some(&current) {
            bail!("{} already exists or has local changes. Move it or keep a private copy before replacing it.", file.display());
        }
    }
    owned.insert(relative.clone(), checksum);
    changes.insert(relative, Some(bytes));
    Ok(())
}

fn publish_shared(folder: &Path, entries: &[ArchiveFile]) -> Result<()> {
    let parent = folder.parent().context("Invalid live shader storage")?;
    fs::create_dir_all(parent)?;
    if !folder.is_dir() {
        let stage = tempfile::tempdir_in(parent)?;
        for item in entries { atomic_write(&safe_child(stage.path(), &item.path)?, &item.bytes)?; }
        if let Err(error) = fs::rename(stage.path(), folder) {
            if !folder.is_dir() { return Err(error.into()); }
        }
    }
    for item in entries {
        let path = safe_child(folder, &item.path)?;
        if sha256(&fs::read(path)?) != sha256(&item.bytes) { bail!("Shared shader files have local changes. Use private shader storage for customized files."); }
    }
    Ok(())
}

fn lock(base: &Path) -> Result<fs::File> {
    let dir = safe_child(base, Path::new(".reshade-manager"))?;
    fs::create_dir_all(&dir)?;
    let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(safe_child(&dir, Path::new("install.lock"))?)?;
    fs2::FileExt::try_lock_exclusive(&file).context("Another ReShade installation is working in this folder")?;
    Ok(file)
}

#[derive(Default, Deserialize, Serialize)]
struct Journal { entries: Vec<Snapshot> }

#[derive(Deserialize, Serialize)]
struct Snapshot {
    relative: PathBuf,
    previous: Option<PathBuf>,
}

fn recover(base: &Path) -> Result<()> {
    let journal_path = safe_child(base, Path::new(JOURNAL))?;
    if !journal_path.is_file() { return Ok(()); }
    let journal: Journal = read_json(&journal_path)?;
    for item in journal.entries.into_iter().rev() {
        let target = safe_child(base, &item.relative)?;
        match item.previous {
            Some(backup) => {
                let bytes = fs::read(safe_child(base, &backup)?)?;
                if !fs::read(&target).is_ok_and(|current| current == bytes) {
                    atomic_write(&target, &bytes)?;
                }
            }
            None if target.is_file() => fs::remove_file(target)?,
            None => {}
        }
    }
    fs::remove_file(&journal_path)?;
    Ok(())
}

fn apply(base: &Path, changes: BTreeMap<PathBuf, Option<Vec<u8>>>, backups: Option<&Path>) -> Result<()> {
    let staging_relative = Path::new(".reshade-manager/rollback");
    let staging = safe_child(base, staging_relative)?;
    fs::create_dir_all(&staging)?;
    let mut journal = Journal::default();
    for (index, relative) in changes.keys().enumerate() {
        let path = safe_child(base, relative)?;
        let backup = if path.exists() {
            if !path.is_file() { bail!("{} is not a regular file.", path.display()); }
            let relative_backup = staging_relative.join(format!("{index}.previous"));
            atomic_write(&safe_child(base, &relative_backup)?, &fs::read(&path)?)?;
            Some(relative_backup)
        } else { None };
        journal.entries.push(Snapshot { relative: relative.clone(), previous: backup });
    }
    let journal_path = safe_child(base, Path::new(JOURNAL))?;
    write_json(&journal_path, &journal)?;
    let result = (|| -> Result<()> {
        if let Some(backup) = backups {
            fs::create_dir_all(backup)?;
            let snapshot = tempfile::Builder::new().prefix("installation-").tempdir_in(backup)?;
            for item in &journal.entries {
                if let Some(file) = &item.previous {
                    atomic_write(&safe_child(snapshot.path(), &item.relative)?, &fs::read(safe_child(base, file)?)?)?;
                }
            }
            let _ = snapshot.keep();
        }
        for (file, bytes) in &changes {
            let path = safe_child(base, file)?;
            match bytes {
                Some(bytes) => {
                    atomic_write(&path, bytes)?;
                    if sha256(&fs::read(&path)?) != sha256(bytes) { bail!("Verification failed for {}", path.display()); }
                }
                None if path.is_file() => fs::remove_file(path)?,
                None => {}
            }
        }
        fs::remove_file(&journal_path)?;
        Ok(())
    })();
    if let Err(error) = result {
        recover(base).with_context(|| format!("{error:#}. Recovery is pending in {}", base.display()))?;
        return Err(error).context("Changes were rolled back");
    }
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovers_interrupted_change_and_keeps_preset() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let stage = base.join(".reshade-manager/rollback");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("x"), "old").unwrap();
        fs::write(base.join("dxgi.dll"), "new").unwrap();
        fs::write(base.join("Preset.ini"), "my settings").unwrap();
        write_json(&base.join(JOURNAL), &Journal { entries: vec![Snapshot { relative: "dxgi.dll".into(), previous: Some(".reshade-manager/rollback/x".into()) }] }).unwrap();
        recover(base).unwrap();
        assert_eq!(fs::read_to_string(base.join("dxgi.dll")).unwrap(), "old");
        assert_eq!(fs::read_to_string(base.join("Preset.ini")).unwrap(), "my settings");
    }

    #[test]
    fn shader_selection_retains_includes_and_textures() {
        let files = ["repo/Shaders/A.fx", "repo/Shaders/B.fx", "repo/Shaders/Common.fxh", "repo/Textures/LUT.png"]
            .into_iter().map(|path| ArchiveFile { path: path.into(), bytes: vec![1] }).collect();
        let package = Package { id: "1".into(), name: "test".into(), description: "".into(), repository: "".into(), url: "".into(), effects: vec![], denied: vec![], required: false, enabled: false, addon: false };
        let choice = catalog::Choice { all: false, effects: BTreeSet::from(["A.fx".into()]) };
        let entries = shader_entries(files, &package, &choice).unwrap();
        assert_eq!(entries.iter().map(|entry| entry.path.clone()).collect::<Vec<_>>(),
            ["Shaders/A.fx", "Shaders/Common.fxh", "Textures/LUT.png"].map(PathBuf::from));
    }

    #[test]
    fn managed_search_paths_keep_the_games_own_paths() {
        let original = "[GENERAL]\nPresetPath=MyPreset.ini\nEffectSearchPaths=Custom,OldShared\n";
        let new = search_path(original, "EffectSearchPaths", &BTreeSet::from(["OldShared".into()]), &BTreeSet::from(["NewShared".into()])).unwrap();
        let ini = parse_ini(&new);
        assert_eq!(ini["general"]["presetpath"], "MyPreset.ini");
        assert_eq!(ini["general"]["effectsearchpaths"], "Custom,NewShared");
    }

    #[cfg(windows)]
    #[test]
    fn installation_lifecycle_uses_cached_packages_and_preserves_two_game_presets() {
        use std::io::{Cursor, Write};
        let Some(fixtures) = std::env::var_os("RESHADE_TEST_FIXTURES").map(PathBuf::from) else { return; };
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let cache = Cache { root: root.join("cache") };
        let package = |id: &str, addon: bool, url: &str| Package { id: id.into(), name: id.into(), description: String::new(),
            repository: String::new(), url: url.into(), effects: vec!["A.fx".into()], denied: vec![], required: !addon, enabled: !addon, addon };
        let shader = package("shader", false, "https://github.com/test/example/archive/main.zip");
        let addon = package("addon", true, "https://github.com/test/example/releases/download/v1/sample.addon64");
        let make_zip = |prefix: &'static [u8], entries: &[(&str, &[u8])]| {
            let mut data = Cursor::new(prefix.to_vec());
            data.set_position(prefix.len() as u64);
            let mut zip = zip::ZipWriter::new(data);
            for (entry, bytes) in entries {
                zip.start_file(*entry, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap().into_inner()
        };
        let normal32 = fs::read(fixtures.join("standard32.dll")).unwrap();
        let normal64 = fs::read(fixtures.join("standard64.dll")).unwrap();
        let addon64 = fs::read(fixtures.join("addon64.dll")).unwrap();
        let standard_archive = make_zip(b"MZ", &[("ReShade32.dll", &normal32), ("ReShade64.dll", &normal64)]);
        let addon_archive = make_zip(b"MZ", &[("ReShade32.dll", &normal32), ("ReShade64.dll", &addon64)]);
        cache.fixture(&runtime_url(false), &standard_archive).unwrap();
        cache.fixture(&runtime_url(true), &addon_archive).unwrap();
        cache.fixture(&shader.url, &make_zip(b"", &[("repo/Shaders/A.fx", b"#include \"Common.fxh\""), ("repo/Shaders/Common.fxh", b"// fixture")])).unwrap();
        cache.fixture(&addon.url, &addon64).unwrap();
        let shaders = [shader];
        let addons = [addon];
        for (name, full, expected) in [("First", false, &normal64), ("Second", true, &addon64)] {
            let base = root.join(name);
            fs::create_dir(&base).unwrap();
            let executable = base.join("Fixture.exe");
            fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            fs::write(base.join("ReShade.ini"), format!("[GENERAL]\nPresetPath={name}.ini\n")).unwrap();
            fs::write(base.join(format!("{name}.ini")), format!("[custom]\nName={name}\n")).unwrap();
            let target = Target { executable, base: base.clone(), arch: Arch::X64, api: Some(Api::Dxgi),
                forced: false, banned: false, notes: vec![], compatibility: BTreeMap::new() };
            let mut selection = Selection::defaults(&shaders);
            if full { selection.addons.insert("addon".into()); }
            let run = |op, acknowledged| execute(&cache, Plan { target: &target, api: Api::Dxgi, addon: full,
                acknowledged, operation: op, selection: &selection, shaders: &shaders, addons: &addons,
                shared: Some(&root.join("shared")), backups: None }, |_| {});
            if full { assert!(run(Operation::Install, false).is_err()); }
            fs::copy(&target.executable, base.join("dxgi.dll")).unwrap();
            assert!(run(Operation::Install, true).is_err());
            assert!(base.join("dxgi.dll").is_file());
            fs::remove_file(base.join("dxgi.dll")).unwrap();
            run(Operation::Install, true).unwrap();
            assert_eq!(&fs::read(base.join("dxgi.dll")).unwrap(), expected);
            assert_eq!(base.join("sample.addon64").exists(), full);
            assert_eq!(status(&target.executable, &base).version.as_deref(), Some("6.8.0.0"));
            assert_eq!(receipt(&base).unwrap().addon, full);
            if !full {
                fs::remove_file(base.join("dxgi.dll")).unwrap();
                run(Operation::Repair, true).unwrap();
                run(Operation::Update, true).unwrap();
            }
            run(Operation::Uninstall, true).unwrap();
            assert!(!base.join("dxgi.dll").exists());
            assert!(!base.join("sample.addon64").exists());
            assert!(fs::read_to_string(base.join("ReShade.ini")).unwrap().contains(&format!("PresetPath={name}.ini")));
            assert_eq!(fs::read_to_string(base.join(format!("{name}.ini"))).unwrap(), format!("[custom]\nName={name}\n"));
            if !full {
                let config = fs::read(base.join("ReShade.ini")).unwrap();
                fs::write(base.join("dxgi.dll"), &normal64).unwrap();
                adopt(&target, false).unwrap();
                assert_eq!(fs::read(base.join("ReShade.ini")).unwrap(), config);
                assert!(!receipt(&base).unwrap().selection_managed);
                run(Operation::Repair, true).unwrap();
                run(Operation::Uninstall, true).unwrap();
            }
        }
        assert_eq!(fs::read_dir(root.join("shared")).unwrap().count(), 1);
        assert_eq!(cache.stats().0, 4);
    }


    #[cfg(windows)]
    #[test]
    fn verifies_official_distribution_layout_when_provided_by_ci() {
        let Some(directory) = std::env::var_os("RESHADE_UPSTREAM_TEST_ASSETS").map(PathBuf::from) else { return; };
        for (filename, full) in [("ReShade_Setup_6.8.0.exe", false), ("ReShade_Setup_6.8.0_Addon.exe", true)] {
            let bytes = fs::read(directory.join(filename)).unwrap();
            verify_distribution(&bytes, full).unwrap();
            for arch in [Arch::X86, Arch::X64] {
                assert!(!extract_runtime(&bytes, arch).unwrap().is_empty());
            }
        }
    }

}
