use crate::{
    converter::{self, Converter, ENGINE_VERSION, Mode},
    native,
};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{OnceLock, atomic::AtomicBool},
    time::Duration,
};
use uuid::Uuid;

pub const EMBEDDED_CRATE_SHA256: &str =
    "fbb4f247ec21be13451293e760f6246e57300ce80208231ab3e0a437a5d96b27";
pub const OFFICIAL_API: &str = "https://crates.io/api/v1/crates/zhconv";
const MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const METADATA_MAX_BYTES: u64 = 2 * 1024 * 1024;
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const CONNECT_TIMEOUT_SECONDS: u64 = 10;
const MAX_REDIRECTS: usize = 5;
const DOWNLOAD_BUFFER_BYTES: usize = 16 * 1024;
const SOURCE_NAME: &str = "ZhConversion.php";
const ARCHIVE_NAME: &str = "official.crate";
const RELEASE_NAME: &str = "release.json";
const BUNDLE_FILES: [&str; 5] = [
    SOURCE_NAME,
    ARCHIVE_NAME,
    RELEASE_NAME,
    "zh-Hant.json",
    "zh-TW.json",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub version: String,
    pub tag: String,
    pub url: String,
    pub digest: String,
    pub size: u64,
    pub published: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub version: String,
    pub digest: String,
    pub directory: PathBuf,
    pub files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    engine_version: String,
    bundle: Option<Bundle>,
}
/// Process-wide replacement for the standard store location, used by tests
/// and self-checks so they never read or write the user's real dictionary state.
static STANDARD_ROOT: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
}
impl Store {
    pub fn standard() -> Result<Self> {
        if let Some(root) = STANDARD_ROOT.get() {
            return Ok(Self { root: root.clone() });
        }
        Ok(Self {
            root: PathBuf::from(std::env::var_os("LOCALAPPDATA").context("找不到本機資料目錄")?)
                .join("SC2TC-Renamer/mediawiki-dictionaries"),
        })
    }
    /// Redirects `Store::standard()` for the rest of the process. Setting the
    /// same path again is accepted; a different path is an error.
    pub fn override_standard_root(root: PathBuf) -> Result<()> {
        let root = native::absolute(&root)?;
        match STANDARD_ROOT.set(root.clone()) {
            Ok(()) => Ok(()),
            Err(_) if STANDARD_ROOT.get() == Some(&root) => Ok(()),
            Err(_) => bail!("字典儲存路徑已在此程序中設定為其他位置"),
        }
    }
    pub fn active(&self) -> Result<Option<Bundle>> {
        safe_directory_path(&self.root)?;
        let path = self.root.join("active.json");
        if !path_exists(&path)? {
            return Ok(None);
        }
        let value: State = serde_json::from_slice(&read_regular(&path, METADATA_MAX_BYTES)?)
            .context("MediaWiki 字典設定格式損毀")?;
        ensure!(
            value.schema == 1 && value.engine_version == ENGINE_VERSION,
            "MediaWiki 字典設定需要不同的程式版本"
        );
        if let Some(bundle) = &value.bundle {
            self.check_bundle_location(bundle)?;
            validate_files(bundle)?;
        }
        Ok(value.bundle)
    }
    pub fn current(&self) -> Result<(String, String)> {
        Ok(self
            .active()?
            .map(|b| (b.version, b.digest))
            .unwrap_or_else(|| (ENGINE_VERSION.to_owned(), EMBEDDED_CRATE_SHA256.to_owned())))
    }
    pub fn stage_bytes(&self, bytes: &[u8], release: &Release) -> Result<Bundle> {
        let result = self.stage_verified(bytes, release);
        if let Err(error) = &result {
            let _ = self.log(
                "stage_failed",
                json!({"version":release.version,"error":error.to_string()}),
            );
        }
        result
    }
    fn stage_verified(&self, bytes: &[u8], release: &Release) -> Result<Bundle> {
        validate_release(release)?;
        ensure!(bytes.len() as u64 == release.size, "字典包大小不符");
        ensure!(
            converter::source_hash(bytes) == release.digest,
            "官方字典包 SHA-256 不符；保留目前版本"
        );
        let source = extract_source(bytes, release)?;
        let tables = converter::parse_mediawiki(&source)?;
        let mut resources = BTreeMap::new();
        resources.insert(SOURCE_NAME.to_owned(), source.clone());
        resources.insert(ARCHIVE_NAME.to_owned(), bytes.to_vec());
        resources.insert(RELEASE_NAME.to_owned(), serde_json::to_vec(release)?);
        for mode in [Mode::ZhHant, Mode::ZhTw] {
            resources.insert(
                mode.config().to_owned(),
                converter::config_bytes(&tables, mode, &converter::source_hash(&source))?,
            );
        }
        safe_directory_path(&self.root)?;
        let parent = self.root.join("bundles");
        safe_directory_path(&parent)?;
        fs::create_dir_all(&parent)?;
        safe_directory_path(&parent)?;
        let directory = parent.join(format!("{}-{}", release.version, release.digest));
        let bundle = Bundle {
            version: release.version.clone(),
            digest: release.digest.clone(),
            directory,
            files: resources
                .iter()
                .map(|(name, data)| (name.clone(), converter::source_hash(data)))
                .collect(),
        };
        let _stage_lock = native::OperationLock::dictionary()?;
        if path_exists(&bundle.directory)? {
            validate_files(&bundle)?;
            self.log(
                "staged_reused",
                json!({"version":bundle.version,"sha256":bundle.digest}),
            )?;
            return Ok(bundle);
        }
        // Unfinished stages stay separate and never become active or block a later retry.
        let mut pending = bundle.clone();
        pending.directory = parent.join(format!(
            "pending-{}-{}",
            release.version,
            Uuid::new_v4().simple()
        ));
        fs::create_dir(&pending.directory)?;
        safe_directory_path(&pending.directory)?;
        self.log(
            "stage_started",
            json!({"version":bundle.version,"sha256":bundle.digest,"directory":pending.directory}),
        )?;
        for (name, data) in resources {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(pending.directory.join(name))?;
            file.write_all(&data)?;
            file.sync_all()?;
        }
        validate_files(&pending)?;
        let identity = native::metadata(&pending.directory)?.identity;
        native::rename_no_replace(&pending.directory, &bundle.directory, &identity)?;
        self.log(
            "staged",
            json!({"version":bundle.version,"sha256":bundle.digest,"files":bundle.files.len()}),
        )?;
        Ok(bundle)
    }
    fn check_bundle_location(&self, bundle: &Bundle) -> Result<()> {
        ensure!(
            bundle.directory.is_absolute()
                && !bundle
                    .directory
                    .components()
                    .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
                && native::contains(&self.root.join("bundles"), &bundle.directory),
            "字典路徑超出工具目錄"
        );
        ensure!(
            bundle.directory.file_name().and_then(|x| x.to_str())
                == Some(format!("{}-{}", bundle.version, bundle.digest).as_str()),
            "字典儲存目錄名稱不符"
        );
        safe_directory_path(&bundle.directory)
    }
    pub fn activate(&self, bundle: &Bundle) -> Result<()> {
        let _lock = native::OperationLock::acquire()?;
        let result = (|| {
            self.check_bundle_location(bundle)?;
            validate_files(bundle)?;
            self.write_state(Some(bundle))?;
            self.log(
                "activated",
                json!({"version":bundle.version,"sha256":bundle.digest}),
            )
        })();
        if let Err(error) = &result {
            let _ = self.log(
                "activation_failed",
                json!({"version":bundle.version,"error":format!("{error:#}")}),
            );
        }
        result
    }
    pub fn reset_embedded(&self) -> Result<()> {
        let _lock = native::OperationLock::acquire()?;
        self.write_state(None)?;
        self.log("reset_embedded", json!({"version":ENGINE_VERSION}))
    }
    fn write_state(&self, bundle: Option<&Bundle>) -> Result<()> {
        safe_directory_path(&self.root)?;
        fs::create_dir_all(&self.root)?;
        safe_directory_path(&self.root)?;
        let state = self.root.join("active.json");
        let id = Uuid::new_v4().simple().to_string();
        if path_exists(&state)? {
            let prior = read_regular(&state, METADATA_MAX_BYTES)?;
            let mut backup = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(format!("active-backup-{id}.json")))?;
            backup.write_all(&prior)?;
            backup.sync_all()?;
        }
        let temporary = self.root.join(format!("active-pending-{id}.json"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(
            &mut file,
            &State {
                schema: 1,
                engine_version: ENGINE_VERSION.to_owned(),
                bundle: bundle.cloned(),
            },
        )?;
        file.sync_all()?;
        drop(file);
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let from = native::wide(temporary.as_os_str());
        let to = native::wide(state.as_os_str());
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn log(&self, event: &str, detail: Value) -> Result<()> {
        safe_directory_path(&self.root)?;
        fs::create_dir_all(&self.root)?;
        let path = self.root.join("updates.jsonl");
        if path_exists(&path)? {
            read_regular(&path, MAX_EXPANDED_BYTES)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(
            file,
            "{}",
            json!({"time":chrono::Utc::now().to_rfc3339(),"event":event,"detail":detail})
        )?;
        file.sync_all()?;
        Ok(())
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn safe_directory_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|p| matches!(p, Component::ParentDir | Component::CurDir)),
        "字典儲存路徑必須是不含相對片段的絕對路徑"
    );
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let metadata = native::metadata(ancestor)?;
                ensure!(
                    !metadata.link && metadata.directory,
                    "字典儲存路徑包含連結或非資料夾項目：{}",
                    ancestor.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    safe_directory_path(path.parent().context("字典檔案缺少資料夾")?)?;
    let info = native::metadata(path)?;
    ensure!(
        !info.link && info.identity.size.is_some_and(|size| size <= limit),
        "字典檔案不是實體檔案或超出上限：{}",
        path.display()
    );
    let mut data = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "字典檔案讀取超出上限");
    Ok(data)
}
fn version(value: &str) -> Result<(u64, u64, u64)> {
    let parts = value.split('.').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|p| !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))),
        "正式版本格式不支援；不接受測試版本"
    );
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}
pub fn validate_release(release: &Release) -> Result<()> {
    version(&release.version)?;
    ensure!(
        release.tag == format!("zhconv-{}", release.version),
        "官方版本標籤格式不符"
    );
    ensure!(
        release.url
            == format!(
                "https://static.crates.io/crates/zhconv/zhconv-{}.crate",
                release.version
            ),
        "字典來源不是指定官方 zhconv 套件"
    );
    ensure!(
        converter::valid_hash(&release.digest)
            && release.digest == release.digest.to_ascii_lowercase(),
        "官方 SHA-256 資訊缺失或格式不符"
    );
    ensure!(
        release.size > 0 && release.size <= MAX_DOWNLOAD_BYTES,
        "官方字典包大小不支援"
    );
    Ok(())
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "SC2TC-Renamer/",
            env!("CARGO_PKG_VERSION"),
            " manual-mediawiki-table-update"
        ))
        .https_only(true)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let allowed = ["crates.io", "static.crates.io"];
            if attempt.previous().len() >= MAX_REDIRECTS
                || !allowed.contains(&attempt.url().host_str().unwrap_or(""))
                || attempt.url().scheme() != "https"
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}
pub fn release_from_metadata(bytes: &[u8]) -> Result<Release> {
    ensure!(
        bytes.len() as u64 <= METADATA_MAX_BYTES,
        "官方版本資訊超出上限"
    );
    let data: Value = serde_json::from_slice(bytes)?;
    ensure!(
        data["crate"]["name"] == "zhconv",
        "不是官方指定套件的版本資訊"
    );
    let versions = data["versions"].as_array().context("官方版本清單缺失")?;
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for item in versions {
        let number = item["num"].as_str().context("官方版本名稱缺失")?;
        ensure!(seen.insert(number), "官方版本清單含有重複版本");
        if item["yanked"].as_bool().context("官方版本撤回狀態缺失")? {
            continue;
        }
        if let Ok(order) = version(number) {
            candidates.push((order, item));
        }
    }
    let (_, item) = candidates
        .into_iter()
        .max_by_key(|(order, _)| *order)
        .context("官方沒有可用的正式版本")?;
    let number = item["num"].as_str().context("官方版本名稱缺失")?;
    ensure!(
        item["dl_path"] == format!("/api/v1/crates/zhconv/{number}/download"),
        "官方下載路徑不符"
    );
    let release = Release {
        version: number.to_owned(),
        tag: format!("zhconv-{number}"),
        url: format!("https://static.crates.io/crates/zhconv/zhconv-{number}.crate"),
        digest: item["checksum"]
            .as_str()
            .context("官方 SHA-256 缺失")?
            .to_lowercase(),
        size: item["crate_size"].as_u64().context("官方字典包大小缺失")?,
        published: item["created_at"]
            .as_str()
            .context("官方發佈日期缺失")?
            .to_owned(),
    };
    validate_release(&release)?;
    Ok(release)
}
pub fn check_official() -> Result<Release> {
    let response = client()?.get(OFFICIAL_API).send()?.error_for_status()?;
    let mut bytes = Vec::new();
    response
        .take(METADATA_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    release_from_metadata(&bytes)
}
pub fn available(store: &Store, release: &Release) -> Result<bool> {
    validate_release(release)?;
    let (current, digest) = store.current()?;
    Ok(version(&release.version)? > version(&current)?
        || (version(&release.version)? == version(&current)? && digest != release.digest))
}
pub fn download_and_stage(
    store: &Store,
    release: &Release,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<Bundle> {
    let result = (|| {
        validate_release(release)?;
        crate::engine::cancelled(cancel)?;
        progress("下載官方 MediaWiki 轉換表來源…".to_owned());
        let response = client()?.get(&release.url).send()?.error_for_status()?;
        let mut bytes = Vec::new();
        let mut source = response.take(MAX_DOWNLOAD_BYTES + 1);
        let mut buffer = [0u8; DOWNLOAD_BUFFER_BYTES];
        loop {
            crate::engine::cancelled(cancel)?;
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            ensure!(bytes.len() as u64 <= MAX_DOWNLOAD_BYTES, "下載超出上限");
            progress(format!(
                "下載轉換表 · {} / {} 位元組",
                bytes.len(),
                release.size
            ));
        }
        crate::engine::cancelled(cancel)?;
        progress("驗證官方 SHA-256、MediaWiki 語法與兩種模式…".to_owned());
        store.stage_bytes(&bytes, release)
    })();
    if let Err(error) = &result {
        let _ = store.log(
            "download_failed",
            json!({"version":release.version,"error":format!("{error:#}")}),
        );
    }
    result
}
fn extract_source(bytes: &[u8], release: &Release) -> Result<Vec<u8>> {
    ensure!(bytes.len() as u64 <= MAX_DOWNLOAD_BYTES, "字典包超出上限");
    let decoder = GzDecoder::new(Cursor::new(bytes)).take(MAX_EXPANDED_BYTES + 1);
    let mut archive = tar::Archive::new(decoder);
    let prefix = format!("zhconv-{}", release.version);
    let target = format!("{prefix}/data/{SOURCE_NAME}");
    let mut source = None;
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let mut count = 0usize;
    for item in archive.entries()? {
        count += 1;
        ensure!(count <= MAX_ARCHIVE_ENTRIES, "官方來源包項目數超出上限");
        let mut item = item?;
        let path = item.path()?.into_owned();
        ensure!(
            !path.is_absolute()
                && path.components().all(|p| matches!(p, Component::Normal(_)))
                && !path.to_string_lossy().contains(['\\', ':'])
                && path
                    .components()
                    .next()
                    .and_then(|p| p.as_os_str().to_str())
                    == Some(prefix.as_str()),
            "官方來源包含有不合法路徑"
        );
        ensure!(seen.insert(path.clone()), "官方來源包含重複路徑");
        ensure!(
            item.header().entry_type().is_file() || item.header().entry_type().is_dir(),
            "官方來源包包含連結或不支援的項目"
        );
        if item.header().entry_type().is_dir() {
            continue;
        }
        let size = item.size();
        total = total.checked_add(size).context("解壓大小計算超出上限")?;
        ensure!(
            size <= MAX_ARCHIVE_FILE_BYTES && total <= MAX_EXPANDED_BYTES,
            "官方來源包解壓超出上限"
        );
        if path.to_str() == Some(target.as_str()) {
            ensure!(
                size <= converter::MAX_SOURCE_BYTES,
                "MediaWiki 來源超出上限"
            );
            let mut data = Vec::new();
            item.take(converter::MAX_SOURCE_BYTES + 1)
                .read_to_end(&mut data)?;
            ensure!(data.len() as u64 == size, "MediaWiki 來源檔案不完整");
            source = Some(data);
        } else {
            std::io::copy(&mut item, &mut std::io::sink())?;
        }
    }
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    ensure!(decoder.limit() > 0, "官方來源包解壓超出上限");
    source.context("官方套件沒有提供支援的 data/ZhConversion.php")
}
pub fn validate_files(bundle: &Bundle) -> Result<()> {
    safe_directory_path(&bundle.directory)?;
    version(&bundle.version)?;
    ensure!(converter::valid_hash(&bundle.digest), "字典包雜湊格式不符");
    let expected = BUNDLE_FILES.iter().copied().collect::<BTreeSet<_>>();
    ensure!(
        bundle
            .files
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == expected,
        "字典包檔案清單不符"
    );
    let mut resources = BTreeMap::new();
    for (name, expected_hash) in &bundle.files {
        ensure!(converter::valid_hash(expected_hash), "字典檔案雜湊格式不符");
        let limit = match name.as_str() {
            ARCHIVE_NAME => MAX_DOWNLOAD_BYTES,
            SOURCE_NAME => converter::MAX_SOURCE_BYTES,
            RELEASE_NAME => METADATA_MAX_BYTES,
            _ => converter::MAX_CONFIG_BYTES,
        };
        let bytes = read_regular(&bundle.directory.join(name), limit)?;
        ensure!(
            converter::source_hash(&bytes) == *expected_hash,
            "字典包內容已變動，保留原設定"
        );
        resources.insert(name.as_str(), bytes);
    }
    let release: Release = serde_json::from_slice(&resources[RELEASE_NAME])?;
    validate_release(&release)?;
    ensure!(
        release.version == bundle.version
            && release.digest == bundle.digest
            && resources[ARCHIVE_NAME].len() as u64 == release.size
            && bundle.files[ARCHIVE_NAME] == bundle.digest,
        "字典包來源紀錄與官方雜湊不一致"
    );
    let source = extract_source(&resources[ARCHIVE_NAME], &release)?;
    ensure!(
        source == resources[SOURCE_NAME],
        "MediaWiki 來源與官方字典包不一致"
    );
    let tables = converter::parse_mediawiki(&source)?;
    let source_sha256 = converter::source_hash(&source);
    for mode in [Mode::ZhHant, Mode::ZhTw] {
        ensure!(
            converter::config_bytes(&tables, mode, &source_sha256)? == resources[mode.config()],
            "模式規則與已驗證 MediaWiki 來源不一致"
        );
        let converter = Converter::from_config(&bundle.directory.join(mode.config()))?;
        ensure!(
            converter.convert("MediaWiki 123")? == "MediaWiki 123",
            "新表相容性檢查未通過"
        );
    }
    Ok(())
}
