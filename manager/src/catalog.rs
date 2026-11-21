use crate::{pe::Arch, storage::{atomic_write, parse_ini, read_json, sha256, write_json, Ini}};
use anyhow::{bail, Context, Result};
use reqwest::{blocking::Client, Url};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs, io::{Cursor, Read}, path::{Path, PathBuf}, time::Duration};

pub const BASE: &str = "https://raw.githubusercontent.com/crosire/reshade-shaders/list/";
pub const MAX_ARCHIVE: u64 = 196 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub description: String,
    pub repository: String,
    pub url: String,
    pub effects: Vec<String>,
    pub denied: Vec<String>,
    pub required: bool,
    pub enabled: bool,
    pub addon: bool,
}

pub fn packages(ini: Ini, addon: Option<Arch>) -> Vec<Package> {
    let list = |value: Option<&String>| value.into_iter().flat_map(|v| v.split(','))
        .map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect();
    ini.into_iter().filter_map(|(id, fields)| {
        let name = fields.get("packagename")?.clone();
        let repository = fields.get("repositoryurl").cloned().unwrap_or_default();
        let url = addon.and_then(|arch| fields.get(&format!("downloadurl{}", arch.number())))
            .or_else(|| fields.get("downloadurl"))?.clone();
        let parsed = Url::parse(&url).ok()?;
        if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") { return None; }
        let repository_ok = Url::parse(&repository).is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("github.com"));
        if !repository_ok { return None; }
        Some(Package {
            id, name, description: fields.get("packagedescription").cloned().unwrap_or_default(),
            repository, url, effects: list(fields.get("effectfiles")), denied: list(fields.get("denyeffectfiles")),
            required: fields.get("required").is_some_and(|v| v == "1"),
            enabled: fields.get("enabled").is_some_and(|v| v == "1"), addon: addon.is_some(),
        })
    }).collect()
}

pub fn addon_category(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    if ["capture", "clipboard", "screenshot", "obs"].iter().any(|term| name.contains(term)) { "Capture" }
    else if ["frame", "display", "window", "swap chain"].iter().any(|term| name.contains(term)) { "Display" }
    else if ["depth", "hdr", "shader"].iter().any(|term| name.contains(term)) { "Rendering" }
    else if ["overlay", "input", "freepie"].iter().any(|term| name.contains(term)) { "Input & overlays" }
    else { "Tools & miscellaneous" }
}

pub fn category(effect: &str) -> &'static str {
    let effect = effect.to_ascii_lowercase();
    for (name, tokens) in [
        ("Lighting & ambient occlusion", &["bloom", "ambient", "mxao", "ssao", "lighting", "light"][..]),
        ("Anti-aliasing", &["smaa", "fxaa", "anti-alias"][..]),
        ("HDR & tone mapping", &["hdr", "tonemap", "aces"][..]),
        ("Sharpening", &["sharpen", "finesharp", "cas.fx"][..]),
        ("Depth effects", &["dof", "depth", "fog"][..]),
        ("CRT & retro", &["crt", "retro", "scanline", "pixel", "nostalgia"][..]),
        ("Film & post-processing", &["film", "grain", "vignette", "glitch"][..]),
        ("Color correction & grading", &["color", "colour", "lut", "curves", "levels", "graded", "balance", "sepia", "saturation"][..]),
    ] {
        if tokens.iter().any(|token| effect.contains(token)) { return name; }
    }
    "Utility & miscellaneous"
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Selection {
    pub shaders: BTreeMap<String, Choice>,
    pub addons: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Choice {
    pub all: bool,
    pub effects: BTreeSet<String>,
}

impl Selection {
    pub fn defaults(packages: &[Package]) -> Self {
        Self { shaders: packages.iter().filter(|p| p.required || p.enabled)
            .map(|p| (p.id.clone(), Choice { all: true, effects: BTreeSet::new() })).collect(), addons: BTreeSet::new() }
    }
    pub fn effect_selected(&self, package: &Package, effect: &str) -> bool {
        package.required || self.shaders.get(&package.id).is_some_and(|choice| choice.all || choice.effects.contains(effect))
    }
    pub fn set_effect(&mut self, package: &Package, effect: &str, enabled: bool) {
        if package.required { return; }
        let choice = self.shaders.entry(package.id.clone()).or_default();
        if choice.all {
            choice.effects = package.effects.iter().cloned().collect();
            choice.all = false;
        }
        if enabled { choice.effects.insert(effect.to_owned()); } else { choice.effects.remove(effect); }
        if choice.effects.is_empty() { self.shaders.remove(&package.id); }
    }
    pub fn set_all(&mut self, package: &Package, enabled: bool) {
        if enabled || package.required {
            self.shaders.insert(package.id.clone(), Choice { all: true, effects: BTreeSet::new() });
        } else { self.shaders.remove(&package.id); }
    }
}

pub fn gog_image_host(host: &str) -> bool {
    matches!(host, "images.gog-statics.com" | "images-1.gog-statics.com" | "images-2.gog-statics.com"
        | "images-3.gog-statics.com" | "images-4.gog-statics.com")
}

pub fn client() -> Result<Client> { client_with_timeout(Duration::from_secs(45)) }

pub fn client_with_timeout(timeout: Duration) -> Result<Client> {
    Ok(Client::builder().user_agent("ReShade-Manager")
        .connect_timeout(Duration::from_secs(5)).timeout(timeout)
        .https_only(true).redirect(reqwest::redirect::Policy::custom(|attempt| {
            let trusted = attempt.url().host_str().is_some_and(|host| matches!(host, "reshade.me" | "www.reshade.me" | "github.com" | "api.github.com" | "raw.githubusercontent.com" | "codeload.github.com" | "release-assets.githubusercontent.com" | "objects.githubusercontent.com" | "cdn.akamai.steamstatic.com" | "api.gog.com") || gog_image_host(host));
            if trusted && attempt.previous().len() < 8 { attempt.follow() } else { attempt.error("Untrusted redirect destination") }
        })).build()?)
}

pub fn download(client: &Client, url: &str, max: u64) -> Result<Vec<u8>> {
    let response = client.get(url).send().with_context(|| format!("Download {url}"))?.error_for_status()?;
    if response.content_length().is_some_and(|length| length > max) { bail!("Download exceeds size limit"); }
    let mut result = Vec::new();
    response.take(max + 1).read_to_end(&mut result)?;
    if result.len() as u64 > max { bail!("Download exceeds size limit"); }
    Ok(result)
}

pub fn metadata(client: &Client, location: &Path, name: &str) -> Result<Ini> {
    let path = location.join(name);
    match download(client, &format!("{BASE}{name}"), 2 * 1024 * 1024)
        .and_then(|b| String::from_utf8(b).context("Invalid catalog encoding")) {
        Ok(text) if !parse_ini(&text).is_empty() => {
            atomic_write(&path, text.as_bytes())?;
            Ok(parse_ini(&text))
        }
        _ => Ok(parse_ini(&fs::read_to_string(&path).with_context(|| format!("{name} is unavailable online and in the local cache"))?)),
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Cached {
    pub digest: String,
    pub version: String,
    pub url: String,
    pub size: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct Receipts(BTreeMap<String, Cached>);

pub struct Cache {
    pub root: PathBuf,
}

impl Cache {
    fn lock(&self) -> Result<fs::File> {
        fs::create_dir_all(&self.root)?;
        let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(self.root.join(".lock"))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    fn index(&self) -> PathBuf { self.root.join("packages.json") }

    fn file(&self, digest: &str) -> Result<PathBuf> {
        if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) { bail!("Invalid package checksum"); }
        Ok(self.root.join(format!("{digest}.package")))
    }

    fn verify(&self, entry: &Cached) -> Result<Vec<u8>> {
        let path = self.file(&entry.digest)?;
        if path.metadata()?.len() != entry.size || entry.size > MAX_ARCHIVE { bail!("Cached package size changed"); }
        let bytes = fs::read(path)?;
        if sha256(&bytes) != entry.digest { bail!("Cached package failed integrity verification"); }
        Ok(bytes)
    }

    #[cfg(all(test, windows))]
    pub(crate) fn fixture(&self, url: &str, bytes: &[u8]) -> Result<()> {
        let _lock = self.lock()?;
        let mut index: Receipts = read_json(&self.index())?;
        let digest = sha256(bytes);
        let entry = Cached { digest, version: "fixture".into(), url: url.into(), size: bytes.len() as u64 };
        atomic_write(&self.file(&entry.digest)?, bytes)?;
        index.0.insert(url.into(), entry);
        write_json(&self.index(), &index)
    }

    pub fn contains(&self, url: &str) -> bool {
        self.status(url).is_some()
    }

    pub fn status(&self, url: &str) -> Option<Cached> {
        let index = read_json::<Receipts>(&self.index()).ok()?;
        let entry = index.0.get(url)?;
        self.file(&entry.digest).ok().filter(|p| p.is_file()).map(|_| entry.clone())
    }

    pub fn obtain(&self, client: &Client, url: &str, shader: bool, update: bool, validate: impl Fn(&[u8]) -> Result<()>) -> Result<(Cached, Vec<u8>)> {
        let _lock = self.lock()?;
        let mut entries: Receipts = read_json(&self.index())?;
        let current = entries.0.get(url);
        if !update {
            if let Some((entry, bytes)) = current.and_then(|e| self.verify(e).ok().map(|b| (e, b))) {
                validate(&bytes)?;
                return Ok((entry.clone(), bytes));
            }
        }
        let (resolved, version, expected_digest) = if shader {
            match resolve_shader(client, url) {
                Ok((url, version)) => (url, version, None),
                Err(error) => {
                    if let Some((entry, bytes)) = current.and_then(|e| self.verify(e).ok().map(|b| (e, b))) {
                        validate(&bytes)?;
                        return Ok((entry.clone(), bytes));
                    }
                    return Err(error);
                }
            }
        } else if url.starts_with("https://github.com/") {
            match resolve_release(client, url) {
                Ok(release) => release,
                Err(error) => {
                    if let Some((entry, bytes)) = current.and_then(|e| self.verify(e).ok().map(|b| (e, b))) {
                        validate(&bytes)?;
                        return Ok((entry.clone(), bytes));
                    }
                    return Err(error);
                }
            }
        } else { (url.to_owned(), "distribution".to_owned(), None) };
        if let Some((entry, bytes)) = current.filter(|e| e.url == resolved && (shader || !update))
            .and_then(|e| self.verify(e).ok().map(|b| (e, b))) {
            validate(&bytes)?;
            return Ok((entry.clone(), bytes));
        }
        let bytes = download(client, &resolved, MAX_ARCHIVE)?;
        validate(&bytes)?;
        let digest = sha256(&bytes);
        if expected_digest.as_ref().is_some_and(|expected| !expected.eq_ignore_ascii_case(&digest)) {
            bail!("Download failed the publisher's SHA-256 digest check.");
        }
        let entry = Cached { digest, version, url: resolved, size: bytes.len() as u64 };
        atomic_write(&self.file(&entry.digest)?, &bytes)?;
        entries.0.insert(url.to_owned(), entry.clone());
        write_json(&self.index(), &entries)?;
        Ok((entry, bytes))
    }

    pub fn stats(&self) -> (usize, u64) {
        let entries = self.files();
        (entries.len(), entries.iter().map(|(_, size, _)| size).sum())
    }

    pub fn trim(&self, limit: u64) -> Result<()> {
        let _lock = self.lock()?;
        let mut files = self.files();
        files.sort_unstable();
        let mut size: u64 = files.iter().map(|(_, size, _)| size).sum();
        for (_, length, path) in files {
            if size <= limit { break; }
            fs::remove_file(path)?;
            size -= length;
        }
        let mut entries: Receipts = read_json(&self.index())?;
        entries.0.retain(|_, entry| self.file(&entry.digest).is_ok_and(|p| p.is_file()));
        write_json(&self.index(), &entries)
    }

    fn files(&self) -> Vec<(std::time::SystemTime, u64, PathBuf)> {
        fs::read_dir(&self.root).into_iter().flatten().filter_map(Result::ok).filter_map(|item| {
            let path = item.path();
            let meta = item.metadata().ok()?;
            (path.extension().is_some_and(|s| s == "package") && meta.is_file())
                .then(|| (meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), meta.len(), path))
        }).collect()
    }
}

fn resolve_release(client: &Client, url: &str) -> Result<(String, String, Option<String>)> {
    let url = Url::parse(url)?;
    if url.host_str() != Some("github.com") { bail!("Unsupported add-on source"); }
    let parts: Vec<_> = url.path_segments().context("Invalid release URL")?.collect();
    let (owner, repo, ref_path, asset) = match parts.as_slice() {
        [owner, repo, "releases", "latest", "download", asset] => (*owner, *repo, "latest".to_owned(), *asset),
        [owner, repo, "releases", "download", tag, asset] => (*owner, *repo, format!("tags/{tag}"), *asset),
        _ => bail!("Unsupported add-on release path"),
    };
    let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/{ref_path}");
    let response: serde_json::Value = serde_json::from_slice(&download(client, &api, 2 * 1024 * 1024)?)?;
    let tag = response["tag_name"].as_str().context("The add-on release has no version")?;
    let assets = response["assets"].as_array().context("The add-on release has no assets")?;
    let resolved = assets.iter().find(|entry| entry["browser_download_url"].as_str().is_some_and(|url| url.ends_with(&format!("/{asset}"))))
        .context("The configured add-on asset was not in the release")?;
    let download_url = resolved["browser_download_url"].as_str().context("The add-on asset has no download URL")?;
    if !download_url.starts_with(&format!("https://github.com/{owner}/{repo}/releases/download/")) {
        bail!("The add-on asset points outside the configured repository.");
    }
    let digest = resolved["digest"].as_str().and_then(|value| value.strip_prefix("sha256:")).map(str::to_owned);
    Ok((download_url.into(), tag.into(), digest))
}

pub fn available_updates(client: &Client, cache: &Cache, packages: &[Package]) -> Result<(Vec<String>, usize)> {
    let mut updates = Vec::new();
    let mut checked = 0;
    let mut errors = 0;
    for package in packages {
        let Some(cached) = cache.status(&package.url) else { continue; };
        match resolve_shader(client, &package.url) {
            Ok((_, latest)) => {
                checked += 1;
                if latest != cached.version { updates.push(package.name.clone()); }
            }
            Err(_) => errors += 1,
        }
    }
    if checked == 0 && errors != 0 { bail!("GitHub did not return shader repository revisions."); }
    Ok((updates, errors))
}

fn resolve_shader(client: &Client, url: &str) -> Result<(String, String)> {
    let url = Url::parse(url)?;
    if url.host_str() != Some("github.com") { bail!("Unsupported shader package source"); }
    let parts: Vec<_> = url.path_segments().context("Invalid shader URL")?.collect();
    let (owner, repo, branch) = match parts.as_slice() {
        [owner, repo, "archive", branch] | [owner, repo, "archive", "refs", "heads", branch] => (*owner, *repo, *branch),
        _ => bail!("Unsupported shader archive path"),
    };
    let branch = branch.strip_suffix(".zip").context("Expected a shader ZIP archive")?;
    if [owner, repo, branch].iter().any(|piece| piece.is_empty() || !piece.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))) { bail!("Invalid shader repository identifier"); }
    let api = format!("https://api.github.com/repos/{owner}/{repo}/commits/{branch}");
    let json: serde_json::Value = serde_json::from_slice(&download(client, &api, 2 * 1024 * 1024)?)?;
    let sha = json["sha"].as_str().filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())).context("Missing shader revision")?;
    Ok((format!("https://codeload.github.com/{owner}/{repo}/zip/{sha}"), sha.to_owned()))
}

pub struct ArchiveFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

pub fn files(bytes: &[u8], include: impl Fn(&str) -> bool) -> Result<Vec<ArchiveFile>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > 14_000 { bail!("Archive has too many entries"); }
    let mut result = Vec::new();
    let mut total = 0;
    let mut paths = BTreeSet::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() || !include(entry.name()) { continue; }
        let path = entry.enclosed_name().context("Unsafe archive path")?;
        if entry.name().contains('\\') || entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            bail!("Unsupported archive link or path");
        }
        let key = path.to_string_lossy().to_lowercase();
        if !paths.insert(key) { bail!("Archive contains duplicate Windows paths"); }
        total += entry.size();
        if total > 512 * 1024 * 1024 || entry.size() > 100 * 1024 * 1024 { bail!("Archive exceeds extraction limits"); }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        result.push(ArchiveFile { path, bytes });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn individual_changes_preserve_other_effects_and_required_packages() {
        let ini = parse_ini("[00]\nPackageName=Core\nRequired=1\nDownloadUrl=https://github.com/a/b/archive/main.zip\nRepositoryUrl=https://github.com/a/b\nEffectFiles=A.fx\n[01]\nPackageName=Extra\nEnabled=1\nDownloadUrl=https://github.com/a/c/archive/main.zip\nRepositoryUrl=https://github.com/a/c\nEffectFiles=X.fx,Y.fx");
        let packages = packages(ini, None);
        let mut selected = Selection::defaults(&packages);
        selected.set_effect(&packages[0], "A.fx", false);
        selected.set_effect(&packages[1], "X.fx", false);
        assert!(selected.effect_selected(&packages[0], "A.fx"));
        assert!(!selected.effect_selected(&packages[1], "X.fx"));
        assert!(selected.effect_selected(&packages[1], "Y.fx"));
    }

    #[test]
    fn verified_cache_can_be_reused_offline_and_detects_tampering() {
        let d = tempfile::tempdir().unwrap();
        let cache = Cache { root: d.path().to_owned() };
        let data = b"package";
        let url = "https://invalid.invalid/not-accessible";
        let entry = Cached { digest: sha256(data), size: data.len() as u64, ..Cached::default() };
        let path = cache.file(&entry.digest).unwrap();
        fs::write(&path, data).unwrap();
        write_json(&cache.index(), &Receipts(BTreeMap::from([(url.into(), entry.clone())]))).unwrap();
        let (_, result) = cache.obtain(&client().unwrap(), url, false, false, |_| Ok(())).unwrap();
        assert_eq!(result, data);
        fs::write(path, b"corrupt").unwrap();
        assert!(cache.verify(&entry).is_err());
        cache.trim(0).unwrap();
        assert_eq!(cache.stats(), (0, 0));
    }
}
