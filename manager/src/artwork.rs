use crate::{catalog, discovery::{Game, Source}, storage::{atomic_write, sha256}};
use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, ImageReader, Limits};
use std::{fs, io::Cursor, path::{Path, PathBuf}, time::SystemTime};

pub fn load(game: &Game, custom: Option<&Path>, directory: &Path, limit: u64, refresh: bool) -> Result<PathBuf> {
    let key = format!("{}:{}", game.key(), custom.map(|p| p.to_string_lossy()).unwrap_or_default());
    let target = directory.join(format!("{}.jpg", sha256(key.as_bytes())));
    if !refresh && target.is_file() { return Ok(target); }
    let local = custom.into_iter().chain(game.artwork_paths.iter().map(PathBuf::as_path))
        .filter_map(|path| {
            if path.metadata().ok()?.len() > 16 * 1024 * 1024 { return None; }
            thumbnail(&fs::read(path).ok()?).ok()
        }).next();
    let result = local.or_else(|| online_cover(game)).or_else(|| {
        let icon = executable_icon(&game.executable)?;
        encode(image::DynamicImage::ImageRgba8(icon)).ok()
    });
    if let Some(result) = result {
        atomic_write(&target, &result)?;
        trim(directory, limit, Some(&target));
        Ok(target)
    } else if target.is_file() {
        Ok(target)
    } else {
        anyhow::bail!("No artwork is available for {}", game.title)
    }
}

fn online_cover(game: &Game) -> Option<Vec<u8>> {
    let id = game.store_id.as_deref().filter(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()))?;
    let client = catalog::client_with_timeout(std::time::Duration::from_secs(5)).ok()?;
    let url = match game.source {
        Source::Steam => format!("https://cdn.akamai.steamstatic.com/steam/apps/{id}/library_600x900.jpg"),
        Source::Gog => {
            let url = format!("https://api.gog.com/products/{id}");
            let metadata: serde_json::Value = serde_json::from_slice(&catalog::download(&client, &url, 512 * 1024).ok()?).ok()?;
            if metadata["id"].as_u64() != id.parse::<u64>().ok() && metadata["id"].as_str() != Some(id) { return None; }
            let image = ["boxArtImage", "background", "icon"].iter().find_map(|key| metadata["images"][key].as_str())?;
            let image = if image.starts_with("//") { format!("https:{image}") } else { image.to_owned() };
            let parsed = reqwest::Url::parse(&image).ok()?;
            if parsed.scheme() != "https" || !parsed.host_str().is_some_and(catalog::gog_image_host) { return None; }
            image
        }
        _ => return None,
    };
    thumbnail(&catalog::download(&client, &url, 16 * 1024 * 1024).ok()?).ok()
}

fn thumbnail(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    encode(reader.decode()?.thumbnail(480, 480))
}

fn encode(image: image::DynamicImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 84).encode_image(&image.to_rgb8()).context("Encode artwork")?;
    Ok(bytes)
}

pub fn trim(directory: &Path, limit: u64, keep: Option<&Path>) {
    let mut entries: Vec<_> = fs::read_dir(directory).into_iter().flatten().filter_map(Result::ok)
        .filter_map(|item| {
            let metadata = item.metadata().ok()?;
            (metadata.is_file() && item.path().extension().is_some_and(|ext| ext == "jpg"))
                .then(|| (metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), metadata.len(), item.path()))
        }).collect();
    let mut size: u64 = entries.iter().map(|item| item.1).sum();
    entries.sort_unstable();
    for (_, length, file) in entries {
        if size <= limit { break; }
        if Some(file.as_path()) != keep && fs::remove_file(file).is_ok() { size -= length; }
    }
}

#[cfg(windows)]
fn executable_icon(path: &Path) -> Option<image::RgbaImage> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr, slice};
    use windows_sys::Win32::{Graphics::Gdi::*, UI::{Shell::ExtractIconExW, WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL}}};
    const SIZE: i32 = 96;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let mut icon = ptr::null_mut();
        if ExtractIconExW(path.as_ptr(), 0, &mut icon, ptr::null_mut(), 1) == 0 || icon.is_null() { return None; }
        let dc = CreateCompatibleDC(ptr::null_mut());
        let result = (|| {
            if dc.is_null() { return None; }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = SIZE;
            info.bmiHeader.biHeight = -SIZE;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = ptr::null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() { return None; }
            let old = SelectObject(dc, bitmap);
            let data = slice::from_raw_parts_mut(bits.cast::<u8>(), (SIZE * SIZE * 4) as usize);
            for pixel in data.chunks_exact_mut(4) { pixel.copy_from_slice(&[44, 31, 25, 255]); }
            let drew = DrawIconEx(dc, 0, 0, icon, SIZE, SIZE, 0, ptr::null_mut(), DI_NORMAL) != 0;
            for pixel in data.chunks_exact_mut(4) { pixel.swap(0, 2); pixel[3] = 255; }
            let result = drew.then(|| image::RgbaImage::from_raw(SIZE as u32, SIZE as u32, data.to_vec())).flatten();
            SelectObject(dc, old);
            DeleteObject(bitmap);
            result
        })();
        if !dc.is_null() { DeleteDC(dc); }
        DestroyIcon(icon);
        result
    }
}
#[cfg(not(windows))]
fn executable_icon(_: &Path) -> Option<image::RgbaImage> { None }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_custom_art_falls_back_to_launcher_cache() {
        let folder = tempfile::tempdir().unwrap();
        let cover = folder.path().join("cover.png");
        image::RgbImage::new(16, 24).save(&cover).unwrap();
        let game = Game { source: Source::Custom, title: "Fixture".into(), executable: folder.path().join("not-present.exe"), store_id: None, artwork_paths: vec![cover] };
        let path = load(&game, Some(&folder.path().join("missing.png")), &folder.path().join("cache"), 1024 * 1024, false).unwrap();
        assert!(path.is_file());
    }
}
