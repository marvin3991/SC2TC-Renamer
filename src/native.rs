use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    mem,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::Threading::*};

pub const REPARSE_POINT: u32 = FILE_ATTRIBUTE_REPARSE_POINT;
pub const HIDDEN_SYSTEM: u32 = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
pub const WINDOWS_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
pub const NANOS_PER_FILETIME_TICK: i128 = 100;
pub const MAX_COMPONENT_UNITS: usize = 255;
pub const MAX_PATH_UNITS: usize = 32_767;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub volume: u64,
    pub file_id: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_ticks: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Metadata {
    pub identity: Identity,
    pub attributes: u32,
    pub directory: bool,
}

pub struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    if path.to_str().is_none() {
        bail!("路徑不是有效的 Unicode，已停止。");
    }
    Ok(path)
}

pub fn text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("路徑不是有效的 Unicode。")
}

pub fn key(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        value = format!("\\\\{rest}");
    } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
        value = rest.to_owned();
    }
    value.trim_end_matches('\\').to_lowercase()
}

pub fn contains(parent: &Path, child: &Path) -> bool {
    let p = key(parent);
    let c = key(child);
    c == p || c.starts_with(&(p + "\\"))
}

fn extended(path: &Path) -> PathBuf {
    let value = path.to_string_lossy().replace('/', "\\");
    if value.starts_with("\\\\?\\") {
        PathBuf::from(value)
    } else if let Some(rest) = value.strip_prefix("\\\\") {
        PathBuf::from(format!("\\\\?\\UNC\\{rest}"))
    } else {
        PathBuf::from(format!("\\\\?\\{value}"))
    }
}

pub fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open(path: &Path, access: u32) -> Result<Handle> {
    let name = wide(extended(path).as_os_str());
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("無法讀取：{}", path.display()));
    }
    Ok(Handle(handle))
}

fn from_handle(handle: &Handle) -> Result<Metadata> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut id_info: FILE_ID_INFO = unsafe { mem::zeroed() };
    let (volume, file_id) = if unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            (&mut id_info as *mut FILE_ID_INFO).cast(),
            mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0
    {
        (
            id_info.VolumeSerialNumber,
            u128::from_le_bytes(id_info.FileId.Identifier),
        )
    } else {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow),
        )
    };
    if file_id == 0 {
        bail!("檔案系統無法提供穩定的項目身分，停止自動改名。");
    }
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let regular = !directory && info.dwFileAttributes & REPARSE_POINT == 0;
    let identity = Identity {
        volume,
        file_id,
        size: regular
            .then_some((u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow)),
        modified_ticks: regular.then_some(
            (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
                | u64::from(info.ftLastWriteTime.dwLowDateTime),
        ),
    };
    Ok(Metadata {
        identity,
        attributes: info.dwFileAttributes,
        directory,
    })
}

pub fn metadata(path: &Path) -> Result<Metadata> {
    from_handle(&open(path, FILE_READ_ATTRIBUTES)?)
}

pub fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name.encode_utf16().count() > MAX_COMPONENT_UNITS
    {
        return false;
    }
    if name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c)) {
        return false;
    }
    let first = name.split('.').next().unwrap_or("").to_uppercase();
    if matches!(first.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return false;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = first.strip_prefix(prefix)
            && suffix.chars().count() == 1
            && "123456789¹²³".contains(suffix)
        {
            return false;
        }
    }
    true
}

/// Open the exact source object, verify its identity, and atomically refuse replacement.
pub fn rename_no_replace(source: &Path, destination: &Path, expected: &Identity) -> Result<()> {
    if key(source.parent().context("來源缺少上層資料夾")?)
        != key(destination.parent().context("目標缺少上層資料夾")?)
    {
        bail!("改名必須保留在原本的上層資料夾。");
    }
    if !destination
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(valid_name)
    {
        bail!("目標名稱不符合 Windows 規則。");
    }
    let handle = open(source, DELETE | FILE_READ_ATTRIBUTES)?;
    let current = from_handle(&handle)?;
    if current.attributes & REPARSE_POINT != 0 || &current.identity != expected {
        bail!("項目已變動或被替換，停止：{}", source.display());
    }
    // SetFileInformationByHandle also inspects a NUL-terminated DOS path when
    // RootDirectory is null. Include that terminator in the backing allocation.
    let name = wide(extended(destination).as_os_str());
    let name_units = name.len() - 1;
    if name_units >= MAX_PATH_UNITS {
        bail!("目標路徑超出 Windows 上限。");
    }
    let offset = mem::offset_of!(FILE_RENAME_INFO, FileName);
    let bytes = offset + name.len() * mem::size_of::<u16>();
    let mut aligned = vec![0_u64; bytes.div_ceil(mem::size_of::<u64>())];
    let info = aligned.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = (name_units * mem::size_of::<u16>()) as u32;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        if SetFileInformationByHandle(handle.0, FileRenameInfo, info.cast(), bytes as u32) == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "改名未完成：{} → {}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }
    Ok(())
}

pub struct OperationLock(Handle);
impl OperationLock {
    pub fn acquire() -> Result<Self> {
        // Keep the cross-version operation lock used by earlier applications.
        Self::named("Local\\OpenCCRenamerOperationsV1", 0)
    }
    pub fn dictionary() -> Result<Self> {
        const INITIALISATION_TIMEOUT_MS: u32 = 30_000;
        Self::named(
            "Local\\SC2TC-RenamerDictionaryInitV1",
            INITIALISATION_TIMEOUT_MS,
        )
    }
    fn named(name: &str, timeout: u32) -> Result<Self> {
        let name = wide(OsStr::new(name));
        let raw = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let handle = Handle(raw);
        let result = unsafe { WaitForSingleObject(raw, timeout) };
        if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
            bail!("另一個視窗正在改名或復原，請稍後再試。");
        }
        Ok(Self(handle))
    }
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0.0);
        }
    }
}
