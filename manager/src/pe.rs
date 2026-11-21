use crate::storage::{safe_child, Ini};
use anyhow::{bail, Context, Result};
use goblin::pe::PE;
use serde::{Deserialize, Serialize};
use std::{fs::File, io::{Read, Seek, SeekFrom}, path::{Path, PathBuf}};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Arch { X86, X64 }
impl Arch {
    pub fn label(self) -> &'static str { match self { Self::X86 => "x86", Self::X64 => "x64" } }
    pub fn number(self) -> u8 { match self { Self::X86 => 32, Self::X64 => 64 } }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Api { D3d9, Dxgi, D3d10, D3d11, D3d12, OpenGl, Vulkan, Ddraw }
impl Api {
    pub const ALL: [Self; 8] = [Self::D3d9, Self::Dxgi, Self::OpenGl, Self::Vulkan, Self::Ddraw, Self::D3d10, Self::D3d11, Self::D3d12];
    pub fn label(self) -> &'static str { match self {
        Self::D3d9 => "Direct3D 9", Self::Dxgi => "Direct3D 10 / 11 / 12 (DXGI)",
        Self::D3d10 => "Direct3D 10 (specific wrapper)", Self::D3d11 => "Direct3D 11 (specific wrapper)",
        Self::D3d12 => "Direct3D 12 (specific wrapper)", Self::OpenGl => "OpenGL",
        Self::Vulkan => "Vulkan", Self::Ddraw => "DirectDraw",
    }}
    pub fn name(self) -> &'static str { match self {
        Self::D3d9 => "d3d9", Self::Dxgi => "dxgi", Self::D3d10 => "d3d10", Self::D3d11 => "d3d11", Self::D3d12 => "d3d12",
        Self::OpenGl => "opengl32", Self::Vulkan => "vulkan", Self::Ddraw => "ddraw",
    }}
    pub fn from_metadata(name: &str, forced: bool) -> Option<Self> { Some(match name.to_ascii_lowercase().as_str() {
        "d3d8" | "d3d9" => Self::D3d9, "d3d10" if forced => Self::D3d10,
        "d3d11" if forced => Self::D3d11, "d3d12" if forced => Self::D3d12,
        "dxgi" | "d3d10" | "d3d11" | "d3d12" => Self::Dxgi, "opengl" => Self::OpenGl,
        "vulkan" => Self::Vulkan, "ddraw" => Self::Ddraw, _ => return None,
    }) }
}

fn machine(value: u16) -> Result<Arch> { match value {
    0x14c => Ok(Arch::X86), 0x8664 => Ok(Arch::X64), _ => bail!("Unsupported executable architecture (0x{value:04x})"),
} }

pub fn architecture(path: &Path) -> Result<Arch> {
    let mut exe = File::open(path)?;
    let mut dos = [0; 64];
    exe.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" { bail!("The file is not a Windows executable"); }
    let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    exe.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 6];
    exe.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0" { bail!("Invalid PE header"); }
    machine(u16::from_le_bytes(header[4..6].try_into()?))
}

pub fn infer_api(imports: &[&str]) -> Option<Api> {
    let imports: Vec<_> = imports.iter().map(|name| name.to_ascii_lowercase()).collect();
    let uses = |prefix: &str| imports.iter().any(|i| i.starts_with(prefix));
    if uses("vulkan-1") { Some(Api::Vulkan) }
    else if uses("dxgi") || uses("d3d1") || imports.iter().any(|i| i.contains("gfsdk")) { Some(Api::Dxgi) }
    else if uses("d3d9") || uses("d3d8") { Some(Api::D3d9) }
    else if uses("opengl32") { Some(Api::OpenGl) }
    else if uses("ddraw") { Some(Api::Ddraw) }
    else { None }
}

#[derive(Debug)]
pub struct Target {
    pub executable: PathBuf,
    pub base: PathBuf,
    pub arch: Arch,
    pub api: Option<Api>,
    pub forced: bool,
    pub banned: bool,
    pub notes: Vec<&'static str>,
    pub compatibility: std::collections::BTreeMap<String, String>,
}

pub fn analyze(executable: &Path, ini: &Ini) -> Result<Target> {
    let mut executable = executable.canonicalize()?;
    let root = executable.parent().context("Invalid executable directory")?;
    if let Some(relative) = resource_string(&executable) {
        if let Ok(path) = safe_child(root, Path::new(&relative)) {
            if path.is_file() { executable = path.canonicalize()?; }
        }
    }
    let root = executable.parent().context("Invalid executable directory")?;
    if let Some(windows) = std::env::var_os("WINDIR").and_then(|path| PathBuf::from(path).canonicalize().ok()) {
        if root.to_string_lossy().to_lowercase().starts_with(&windows.to_string_lossy().to_lowercase()) {
            bail!("Installation inside the Windows directory is prohibited");
        }
    }
    let key = executable.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let compatibility = ini.get(&key).cloned().unwrap_or_default();
    let enabled = |name: &str| compatibility.get(name).is_some_and(|s| s == "1");
    let base = match compatibility.get("installtarget") {
        Some(relative) => safe_child(root, Path::new(relative))?,
        None => root.to_owned(),
    };
    let mut arch = architecture(&executable)?;
    if compatibility.contains_key("installtarget") && compatibility.contains_key("is64bit") {
        arch = if enabled("is64bit") { Arch::X64 } else { Arch::X86 };
    }
    let file = File::open(&executable)?;
    // Read the mapped PE without loading or executing the image.
    let mapped = unsafe { memmap2::Mmap::map(&file)? };
    let parsed = PE::parse(&mapped).context("Read executable imports")?;
    let forced = enabled("forceinstallapi");
    let mut api = compatibility.get("renderapi").and_then(|name| Api::from_metadata(name, forced))
        .or_else(|| infer_api(&parsed.libraries));
    let mut notes = Vec::new();
    if parsed.libraries.iter().any(|s| s.to_ascii_lowercase().starts_with("d3d8")) {
        notes.push("Direct3D 8 requires a compatible D3D8-to-D3D9 wrapper.");
    }
    if let Some(optional) = parsed.header.optional_header {
        let reserve = optional.windows_fields.size_of_stack_reserve;
        if reserve > 0 && reserve < 1_000_000 && key != "gamelaunchhelper.exe" {
            notes.push("The executable reports a small default stack reserve. ReShade may fail to load.");
        }
    }
    if !forced && ["d3d9.dll", "dxgi.dll"].iter().any(|name| version_string(&base.join(name), "ProductName").as_deref() == Some("DXVK")) {
        api = Some(Api::Vulkan);
    }
    let remix = base.join(".trex/NvRemixBridge.exe");
    if !forced && remix.is_file() && architecture(&remix).ok() == Some(Arch::X64) {
        executable = remix.canonicalize()?;
        arch = Arch::X64;
        api = Some(Api::Vulkan);
    }
    if api.is_none() { notes.push("No graphics API could be inferred from the executable. Choose the renderer used to launch the game."); }
    let banned = enabled("banned");
    Ok(Target { executable, base, arch, api, forced, banned, notes, compatibility })
}

#[cfg(windows)]
pub fn version_string(path: &Path, name: &str) -> Option<String> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr, slice};
    #[link(name = "version")]
    extern "system" {
        fn GetFileVersionInfoSizeW(name: *const u16, handle: *mut u32) -> u32;
        fn GetFileVersionInfoW(name: *const u16, handle: u32, size: u32, data: *mut c_void) -> i32;
        fn VerQueryValueW(data: *const c_void, key: *const u16, result: *mut *mut c_void, len: *mut u32) -> i32;
    }
    let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), ptr::null_mut());
        if size == 0 || size > 1_000_000 { return None; }
        let mut buffer = vec![0u32; (size as usize + 3) / 4];
        let data = buffer.as_mut_ptr().cast::<c_void>();
        if GetFileVersionInfoW(path.as_ptr(), 0, size, data) == 0 { return None; }
        let mut value = ptr::null_mut();
        let mut length = 0;
        let translation: Vec<_> = r"\VarFileInfo\Translation".encode_utf16().chain([0]).collect();
        let mut translations = Vec::new();
        if VerQueryValueW(data, translation.as_ptr(), &mut value, &mut length) != 0 && !value.is_null() {
            for bytes in slice::from_raw_parts(value.cast::<u8>(), length as usize).chunks_exact(4) {
                translations.push((u16::from_le_bytes([bytes[0], bytes[1]]), u16::from_le_bytes([bytes[2], bytes[3]])));
            }
        }
        translations.extend([(0x0409, 1200), (0x0409, 1252), (0x0409, 0)]);
        for (language, codepage) in translations {
            let key: Vec<_> = format!(r"\StringFileInfo\{language:04x}{codepage:04x}\{name}").encode_utf16().chain([0]).collect();
            if VerQueryValueW(data, key.as_ptr(), &mut value, &mut length) != 0 && !value.is_null() && length > 1 {
                return Some(String::from_utf16_lossy(slice::from_raw_parts(value.cast::<u16>(), length as usize - 1)));
            }
        }
        None
    }
}
#[cfg(not(windows))]
pub fn version_string(_: &Path, _: &str) -> Option<String> { None }

#[cfg(windows)]
fn resource_string(path: &Path) -> Option<String> {
    use std::{os::windows::ffi::OsStrExt, ptr, slice};
    use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::*};
    let filename: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let module = LoadLibraryExW(filename.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_AS_DATAFILE);
        if module.is_null() { return None; }
        let result = (|| {
            let resource = FindResourceW(module, 201usize as *const u16, 10usize as *const u16);
            if resource.is_null() { return None; }
            let size = SizeofResource(module, resource);
            if !(2..=4096).contains(&size) || size % 2 != 0 { return None; }
            let resource = LoadResource(module, resource);
            if resource.is_null() { return None; }
            let data = LockResource(resource);
            if data.is_null() { return None; }
            let text = slice::from_raw_parts(data.cast::<u16>(), size as usize / 2);
            String::from_utf16(text.strip_suffix(&[0]).unwrap_or(text)).ok()
        })();
        FreeLibrary(module);
        result
    }
}
#[cfg(not(windows))]
fn resource_string(_: &Path) -> Option<String> { None }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn import_precedence() {
        assert_eq!(infer_api(&["d3d9.dll", "OpenGL32.dll", "D3D11.dll"]), Some(Api::Dxgi));
        assert_eq!(infer_api(&["DXGI.dll", "vulkan-1.dll"]), Some(Api::Vulkan));
        assert_eq!(infer_api(&["Kernel32.dll"]), None);
    }
    #[test]
    fn reads_supported_machine_types_and_rejects_other_machines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.exe");
        for (kind, arch) in [(0x14cu16, Some(Arch::X86)), (0x8664, Some(Arch::X64)), (0xaa64, None)] {
            let mut bytes = vec![0; 70];
            bytes[..2].copy_from_slice(b"MZ");
            bytes[60] = 64;
            bytes[64..68].copy_from_slice(b"PE\0\0");
            bytes[68..70].copy_from_slice(&kind.to_le_bytes());
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(architecture(&path).ok(), arch);
        }
    }
}
