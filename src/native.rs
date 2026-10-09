use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    mem,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::Threading::*};

pub const REPARSE_POINT: u32 = FILE_ATTRIBUTE_REPARSE_POINT;
pub const HIDDEN_SYSTEM: u32 = FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM;
/// winnt.h `IsReparseTagNameSurrogate`: only tags with this bit redirect the
/// name to another object (symbolic links, junctions, WSL links). Cloud
/// placeholders (OneDrive), WOF and deduplicated files carry
/// FILE_ATTRIBUTE_REPARSE_POINT without it and are ordinary files or folders.
pub const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;
/// winnt.h `FILE_SUPPORTS_OPEN_BY_FILE_ID` (windows-sys keeps it behind the
/// `Win32_System_SystemServices` feature): set by NTFS and ReFS, whose file IDs
/// survive a rename; absent on FAT, FAT32 and exFAT.
const FILE_SUPPORTS_OPEN_BY_FILE_ID: u32 = 0x0100_0000;
/// FAT, FAT32 and exFAT report file ID 0 for the volume root (fastfat
/// `FatGenerateFileIdFromDirentOffset`, verified on both file systems). The
/// root can never be renamed or replaced, so a fixed sentinel is a stable
/// identity for it. Every other item must still provide a non-zero ID.
pub const VOLUME_ROOT_FILE_ID: u128 = u128::MAX;
pub const WINDOWS_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
pub const NANOS_PER_FILETIME_TICK: i128 = 100;
pub const MAX_COMPONENT_UNITS: usize = 255;
pub const MAX_PATH_UNITS: usize = 32_767;
pub const OPERATION_BUSY: &str = "另一個視窗正在改名或復原，請稍後再試。";
const DICTIONARY_BUSY: &str = "另一個視窗正在更新轉換表，請稍後再試。";
const DEVICE_PATH_UNSUPPORTED: &str =
    "不支援裝置路徑（\\\\.\\）或 NT 物件路徑；請使用一般磁碟或網路路徑：";
/// NT object manager namespace prefix (`\??\`), accepted verbatim by
/// `RtlDosPathNameToNtPathName_U` and never a Win32 path.
const NT_OBJECT_PREFIX: &str = "\\??\\";

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
    /// True only for name-surrogate reparse points (symbolic links, junctions)
    /// or when the reparse tag could not be read; see `REPARSE_TAG_NAME_SURROGATE`.
    pub link: bool,
    pub reparse_tag: u32,
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
    // Win32 normalisation turns `\??\C:\x` into the drive-relative `C:\??\C:\x`,
    // so the NT object prefix must be rejected before it is lost.
    if path
        .as_os_str()
        .to_string_lossy()
        .starts_with(NT_OBJECT_PREFIX)
    {
        bail!("{DEVICE_PATH_UNSUPPORTED}{}", path.display());
    }
    let path = std::path::absolute(path)?;
    if path.to_str().is_none() {
        bail!("路徑不是有效的 Unicode，已停止。");
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::VerbatimDisk(_)
            | Prefix::UNC(..)
            | Prefix::VerbatimUNC(..) => {}
            // A volume without a drive letter is addressed as `\\?\Volume{GUID}\`.
            Prefix::Verbatim(name) if is_volume_guid(name) => {}
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                bail!("{DEVICE_PATH_UNSUPPORTED}{}", path.display());
            }
        },
        _ => bail!("路徑必須包含磁碟代號或網路位置：{}", path.display()),
    }
    Ok(path)
}

pub fn text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("路徑不是有效的 Unicode：{}", path.display()))
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

/// The `\\?\` form of a path: no Win32 normalisation, so names with trailing
/// dots or spaces and very long paths reach the file system verbatim.
/// Works on UTF-16 units, so names that are not valid Unicode (unpaired
/// surrogates) still open the exact item instead of a U+FFFD look-alike.
pub fn verbatim(path: &Path) -> PathBuf {
    const SLASH: u16 = b'/' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let units: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
        .collect();
    let prefix = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let value = if units.starts_with(&prefix("\\\\?\\")) {
        units
    } else if let Some(rest) = units.strip_prefix(prefix("\\\\").as_slice()) {
        [prefix("\\\\?\\UNC\\").as_slice(), rest].concat()
    } else {
        [prefix("\\\\?\\").as_slice(), &units].concat()
    };
    PathBuf::from(OsString::from_wide(&value))
}

/// Length in UTF-16 units as the kernel sees it (with the `\\?\` prefix).
pub fn path_units(path: &Path) -> usize {
    verbatim(path).as_os_str().encode_wide().count()
}

/// True for `D:\`, `\\?\D:\` and `\\server\share`: the item that FAT family
/// file systems report with file ID 0.
pub fn is_volume_root(path: &Path) -> bool {
    path.parent().is_none()
}

pub fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open(path: &Path, access: u32) -> Result<Handle> {
    let name = wide(verbatim(path).as_os_str());
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

/// Classifies a reparse point: only name surrogates redirect to another object.
pub fn is_name_surrogate(reparse_tag: u32) -> bool {
    reparse_tag & REPARSE_TAG_NAME_SURROGATE != 0
}

/// Resolves the identity of an item whose file system reported ID 0.
pub fn resolve_file_id(file_id: u128, directory: bool, volume_root: bool) -> Result<u128> {
    if file_id != 0 {
        Ok(file_id)
    } else if directory && volume_root {
        Ok(VOLUME_ROOT_FILE_ID)
    } else {
        bail!("檔案系統無法提供穩定的項目身分，停止自動改名。")
    }
}

fn from_handle(handle: &Handle, volume_root: bool) -> Result<Metadata> {
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
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let file_id = resolve_file_id(file_id, directory, volume_root)?;
    let (link, reparse_tag) = if info.dwFileAttributes & REPARSE_POINT == 0 {
        (false, 0)
    } else {
        let mut tag_info: FILE_ATTRIBUTE_TAG_INFO = unsafe { mem::zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileAttributeTagInfo,
                (&mut tag_info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } != 0
        {
            (is_name_surrogate(tag_info.ReparseTag), tag_info.ReparseTag)
        } else {
            // Unknown reparse data is treated as a link so nothing is renamed.
            (true, 0)
        }
    };
    let regular = !directory && !link;
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
        link,
        reparse_tag,
    })
}

pub fn metadata(path: &Path) -> Result<Metadata> {
    from_handle(&open(path, FILE_READ_ATTRIBUTES)?, is_volume_root(path))
}

/// `Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`: the verbatim prefix of a
/// volume mounted without a drive letter.
pub fn is_volume_guid(name: &OsStr) -> bool {
    const GUID_LEN: usize = 36;
    let Some(name) = name.to_str() else {
        return false;
    };
    name.strip_prefix("Volume{")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|guid| {
            guid.len() == GUID_LEN && guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        })
}

/// True when the volume holding `path` assigns a new file ID on rename.
/// NTFS and ReFS report FILE_SUPPORTS_OPEN_BY_FILE_ID and keep IDs stable;
/// FAT, FAT32 and exFAT do not (measured on both), so recovery may fall back
/// to size and timestamp comparison only there.
pub fn volume_renumbers_on_rename(path: &Path) -> Result<bool> {
    let handle = open(path, FILE_READ_ATTRIBUTES)?;
    let mut flags: u32 = 0;
    if unsafe {
        GetVolumeInformationByHandleW(
            handle.0,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut flags,
            ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("無法讀取磁碟區資訊：{}", path.display()));
    }
    Ok(flags & FILE_SUPPORTS_OPEN_BY_FILE_ID == 0)
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
    let current = from_handle(&handle, is_volume_root(source))?;
    if current.link || &current.identity != expected {
        bail!("項目已變動或被替換，停止：{}", source.display());
    }
    // SetFileInformationByHandle also inspects a NUL-terminated DOS path when
    // RootDirectory is null. Include that terminator in the backing allocation.
    let name = wide(verbatim(destination).as_os_str());
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
        Self::named("Local\\OpenCCRenamerOperationsV1", 0, OPERATION_BUSY)
    }
    pub fn dictionary() -> Result<Self> {
        const INITIALISATION_TIMEOUT_MS: u32 = 30_000;
        Self::dictionary_with_timeout(INITIALISATION_TIMEOUT_MS)
    }
    /// `dictionary()` with a caller-chosen wait, so tests can observe the busy
    /// message without waiting for the full initialisation timeout.
    #[doc(hidden)]
    pub fn dictionary_with_timeout(timeout_ms: u32) -> Result<Self> {
        Self::named(
            "Local\\SC2TC-RenamerDictionaryInitV1",
            timeout_ms,
            DICTIONARY_BUSY,
        )
    }
    fn named(name: &str, timeout: u32, busy: &str) -> Result<Self> {
        let name = wide(OsStr::new(name));
        let raw = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error()).context("無法建立作業鎖");
        }
        let handle = Handle(raw);
        match unsafe { WaitForSingleObject(raw, timeout) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            WAIT_TIMEOUT => bail!("{busy}"),
            _ => Err(std::io::Error::last_os_error()).context("等待作業鎖失敗"),
        }
    }
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0.0);
        }
    }
}
