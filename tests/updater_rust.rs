use anyhow::Result;
use flate2::{Compression, write::GzEncoder};
use sc2tc_renamer::{
    converter::{self, Converter, ENGINE_VERSION, Mode},
    updater::{self, Release, Store},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Once, atomic::AtomicBool},
};
use tar::{Builder, EntryType, Header};

/// Prefix shared by both lock-contention messages in `native::OperationLock`;
/// a rejection test must never pass because another process held a lock.
const BUSY_PREFIX: &str = "另一個視窗正在";
/// `std::io::ErrorKind::UnexpectedEof` text that flate2 1.1.10 returns for a
/// truncated or non-gzip stream (observed with the pinned versions in Cargo.lock).
const BAD_GZIP: &str = "unexpected end of file";

/// Points `Store::standard()` at an isolated, empty store for this test binary,
/// so nothing here reads or writes the real %LOCALAPPDATA% dictionary state.
fn isolate() {
    static ISOLATED: Once = Once::new();
    ISOLATED.call_once(|| {
        Store::override_standard_root(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("work/updater_rust-dictionary-store"),
        )
        .unwrap();
    });
}
/// A synthetic directory under work/ that is removed when the test succeeds and
/// kept for inspection when it panics.
struct Fixture(PathBuf);
impl Deref for Fixture {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn fixture() -> Fixture {
    isolate();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/updater-tests")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root).unwrap();
    Fixture(root)
}
fn store(directory: &Fixture) -> Store {
    Store {
        root: directory.to_path_buf(),
    }
}
#[track_caller]
fn rejected<T>(result: Result<T>, needle: &str) {
    let error = match result {
        Ok(_) => panic!("預期失敗並含「{needle}」，但執行成功"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains(needle), "錯誤訊息應含「{needle}」：{error}");
    assert!(
        !error.contains(BUSY_PREFIX),
        "失敗原因不應是作業鎖爭用：{error}"
    );
}
fn embedded() -> (String, String) {
    (
        ENGINE_VERSION.to_owned(),
        updater::EMBEDDED_CRATE_SHA256.to_owned(),
    )
}
fn junction(link: &Path, target: &Path) {
    use std::{os::windows::process::CommandExt, process::Command};
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "chcp 65001 > $null\nNew-Item -ItemType Junction -Path '{}' -Target '{}' -ErrorAction Stop | Out-Null",
        quote(link),
        quote(target)
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn events(store: &Store) -> Vec<String> {
    fs::read_to_string(store.root.join("updates.jsonl"))
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}
fn stale_entries(store: &Store) -> Vec<PathBuf> {
    fs::read_dir(store.root.join("bundles"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("stale-")
        })
        .collect()
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn archive(entries: &[(&str, &[u8], EntryType)]) -> Vec<u8> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (name, data, kind) in entries {
        let mut header = Header::new_gnu();
        // Raw header names deliberately allow traversal fixtures that Builder::append_data refuses.
        assert!(name.len() < 100);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_entry_type(*kind);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("elsewhere").unwrap();
        }
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}
fn bytes() -> Vec<u8> {
    archive(&[(
        "zhconv-0.4.2/data/ZhConversion.php",
        converter::EMBEDDED_SOURCE,
        EntryType::Regular,
    )])
}
fn release(bytes: &[u8]) -> Release {
    Release {
        version: ENGINE_VERSION.to_owned(),
        tag: format!("zhconv-{ENGINE_VERSION}"),
        url: format!("https://static.crates.io/crates/zhconv/zhconv-{ENGINE_VERSION}.crate"),
        digest: hash(bytes),
        size: bytes.len() as u64,
        published: String::new(),
    }
}
#[test]
fn official_table_shape_stages_activates_reuses_and_rolls_back_in_fixture() {
    let bytes = bytes();
    let directory = fixture();
    let store = store(&directory);
    let bundle = store.stage_bytes(&bytes, &release(&bytes)).unwrap();
    assert!(store.active().unwrap().is_none());
    assert_eq!(bundle.files.len(), 5);
    assert_eq!(
        store
            .stage_bytes(&bytes, &release(&bytes))
            .unwrap()
            .directory,
        bundle.directory
    );
    store.activate(&bundle).unwrap();
    assert_eq!(store.current().unwrap().0, ENGINE_VERSION);
    store.reset_embedded().unwrap();
    assert!(store.active().unwrap().is_none());
    assert_eq!(store.current().unwrap().1, updater::EMBEDDED_CRATE_SHA256);
    assert!(fs::read_dir(&store.root).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("active-backup-")
    }));
    let events = events(&store);
    let position = |name: &str| {
        events
            .iter()
            .position(|event| event == name)
            .unwrap_or_else(|| panic!("缺少 {name} 事件：{events:?}"))
    };
    for event in ["stage_started", "staged", "staged_reused"] {
        position(event);
    }
    // Each switch records its intent before active.json changes.
    assert!(position("activate_intent") < position("activated"));
    assert!(position("activated") < position("reset_intent"));
    assert!(position("reset_intent") < position("reset_embedded"));
}
#[test]
fn all_official_source_rules_match_bundled_engine_and_regional_overrides() {
    assert_eq!(
        hash(converter::EMBEDDED_SOURCE),
        converter::EMBEDDED_SOURCE_SHA256
    );
    let tables = converter::parse_mediawiki(converter::EMBEDDED_SOURCE).unwrap();
    // Counts from the pinned source recorded in vendor/mediawiki/manifest.json.
    assert_eq!(tables["ZH_TO_HANT"].len(), 9776);
    assert_eq!(tables["ZH_TO_TW"].len(), 971);
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    for (mode, variant) in [
        (Mode::ZhHant, zhconv::Variant::ZhHant),
        (Mode::ZhTw, zhconv::Variant::ZhTW),
    ] {
        let external = Converter::from_config(&bundle.directory.join(mode.config())).unwrap();
        let builtin = zhconv::get_builtin_converter(variant);
        for (source, target) in tables["ZH_TO_HANT"].iter().chain(tables["ZH_TO_TW"].iter()) {
            for input in [source.as_str(), target.as_str()] {
                assert_eq!(
                    external.convert(input).unwrap(),
                    builtin.convert(input),
                    "{}: {input}",
                    mode.name()
                );
            }
        }
        for input in [
            "岳飞 岳飛",
            "于谦 于謙",
            "张三岳飛文件",
            "软件 数据库 鼠标",
            "頭髮 发展 干杯 皇后 后面",
            "中文🙂測試.txt",
            "ASCII 123",
            "",
        ] {
            assert_eq!(
                external.convert(input).unwrap(),
                builtin.convert(input),
                "{}: {input}",
                mode.name()
            );
        }
        rejected(external.convert("bad\0name"), "名稱含有不合法的空字元");
    }
    let tw = Converter::from_config(&bundle.directory.join(Mode::ZhTw.config())).unwrap();
    assert_eq!(tw.convert("软件 数据库 鼠标").unwrap(), "軟體 資料庫 滑鼠");
}
#[test]
fn bad_hash_size_unofficial_and_prerelease_sources_are_rejected() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let mut candidate = release(&data);
    candidate.digest = "0".repeat(64);
    rejected(
        store.stage_bytes(&data, &candidate),
        "官方字典包 SHA-256 不符",
    );
    assert!(store.active().unwrap().is_none());
    candidate = release(&data);
    candidate.size += 1;
    rejected(store.stage_bytes(&data, &candidate), "字典包大小不符");
    for url in [
        "https://example.com/data.crate",
        "http://static.crates.io/crates/zhconv/zhconv-0.4.2.crate",
        "https://static.crates.io.evil.test/crates/zhconv/zhconv-0.4.2.crate",
        "https://static.crates.io/crates/another/another-0.4.2.crate",
    ] {
        candidate = release(&data);
        candidate.url = url.to_owned();
        rejected(
            updater::validate_release(&candidate),
            "字典來源不是指定官方 zhconv 套件",
        );
    }
    for value in ["0.4.3-beta", "v0.4.2", "00.4.2", "../0.4.2", "0.4"] {
        candidate = release(&data);
        candidate.version = value.to_owned();
        rejected(updater::validate_release(&candidate), "正式版本格式不支援");
    }
    assert!(events(&store).iter().any(|event| event == "stage_failed"));
}
#[test]
fn unsupported_empty_malformed_duplicate_and_executable_php_are_rejected() {
    let source = std::str::from_utf8(converter::EMBEDDED_SOURCE).unwrap();
    let examples = [
        (Vec::new(), "MediaWiki 來源為空或超出上限"),
        (b"not PHP".to_vec(), "預期 <?php"),
        (vec![0xff], "MediaWiki 來源不是 UTF-8"),
        (source.replacen("'㐷' => '傌',", "'㐷' => '傌',\n'㐷' => '另一值',", 1).into_bytes(), "MediaWiki 表含有重複規則"),
        (source.replacen("ZH_TO_HANT", "UNKNOWN_TABLE", 1).into_bytes(), "新版 MediaWiki 有不支援的常數表"),
        (source.replacen("public const", "publicconst", 1).into_bytes(), "預期 public"),
        (source.replacen("namespace MediaWiki", "namespaceMediaWiki", 1).into_bytes(), "預期 namespace"),
        (source.replacen("class ZhConversion {", "class ZhConversion { public const ZH_TO_HANT = ['合成' => '資料'];", 1).into_bytes(), "MediaWiki 表名稱重複"),
        (source.replacen("'㐷' => '傌',", "'㐷' => call_user_func('bad'),", 1).into_bytes(), "預期 '"),
        (source.replacen("'㐷' => '傌',", "'' => '傌',", 1).into_bytes(), "MediaWiki 規則含有空字串"),
        (source.replacen("'㐷' => '傌',", "'㐷' => '',", 1).into_bytes(), "MediaWiki 規則含有空字串"),
        (format!("{source}\neval('malicious');").into_bytes(), "MediaWiki 來源含有未支援的語法"),
        (source.replace("];", "]").into_bytes(), "預期 ;"),
        (b"<?php namespace MediaWiki\\Languages\\Data; class ZhConversion { public const ZH_TO_HANT = []; }".to_vec(), "MediaWiki 常數表不能為空"),
        (format!("{source}\n/*/").into_bytes(), "MediaWiki 註解未結束"),
        (source.replacen("'㐷' => '傌',", "'㐷' => '傌', /*/", 1).into_bytes(), "MediaWiki 註解未結束"),
    ];
    let directory = fixture();
    let store = store(&directory);
    for (source, needle) in examples {
        rejected(converter::parse_mediawiki(&source), needle);
        let data = archive(&[(
            "zhconv-0.4.2/data/ZhConversion.php",
            &source,
            EntryType::Regular,
        )]);
        rejected(store.stage_bytes(&data, &release(&data)), needle);
    }
    assert!(store.active().unwrap().is_none());
}
#[test]
fn php_comments_close_only_after_the_opening_marker() {
    let source = std::str::from_utf8(converter::EMBEDDED_SOURCE).unwrap();
    // The pinned source opens with a `/** ... */` file header.
    assert!(source.starts_with("<?php\n/**"));
    let baseline = converter::parse_mediawiki(converter::EMBEDDED_SOURCE).unwrap();
    for inserted in ["/*/ 'A' => 'B', /*/", "/**/", "/* 'A' => 'B', */"] {
        let commented = source.replacen("'㐷' => '傌',", &format!("'㐷' => '傌', {inserted}"), 1);
        let tables = converter::parse_mediawiki(commented.as_bytes()).unwrap();
        assert!(!tables["ZH_TO_HANT"].contains_key("A"), "{inserted}");
        assert_eq!(tables, baseline, "{inserted}");
    }
}
#[test]
fn archive_traversal_links_duplicates_and_missing_table_are_rejected() {
    let source = converter::EMBEDDED_SOURCE;
    let cases = [
        (
            archive(&[("../outside.php", source, EntryType::Regular)]),
            "官方來源包含有不合法路徑",
        ),
        (
            archive(&[(
                "zhconv-0.4.2/data/../ZhConversion.php",
                source,
                EntryType::Regular,
            )]),
            "官方來源包含有不合法路徑",
        ),
        (
            archive(&[(
                "zhconv-0.4.2/data\\ZhConversion.php",
                source,
                EntryType::Regular,
            )]),
            "官方來源包含有不合法路徑",
        ),
        (
            archive(&[(
                "zhconv-0.4.2/data/ZhConversion.php",
                b"",
                EntryType::Symlink,
            )]),
            "官方來源包包含連結或不支援的項目",
        ),
        (
            archive(&[("zhconv-0.4.2/data/ZhConversion.php", b"", EntryType::Link)]),
            "官方來源包包含連結或不支援的項目",
        ),
        (
            archive(&[
                (
                    "zhconv-0.4.2/data/ZhConversion.php",
                    source,
                    EntryType::Regular,
                ),
                (
                    "zhconv-0.4.2/data/ZhConversion.php",
                    source,
                    EntryType::Regular,
                ),
            ]),
            "官方來源包含重複路徑",
        ),
        (
            archive(&[(
                "another-0.4.2/data/ZhConversion.php",
                source,
                EntryType::Regular,
            )]),
            "官方來源包含有不合法路徑",
        ),
        (
            archive(&[("zhconv-0.4.2/README.md", b"synthetic", EntryType::Regular)]),
            "官方套件沒有提供支援的 data/ZhConversion.php",
        ),
        (b"bad gzip".to_vec(), BAD_GZIP),
    ];
    let directory = fixture();
    let store = store(&directory);
    for (data, needle) in cases {
        rejected(store.stage_bytes(&data, &release(&data)), needle);
    }
    assert!(store.active().unwrap().is_none());
}
#[test]
fn changed_staged_files_or_recomputed_rules_cannot_be_activated() {
    let data = bytes();
    let names = {
        let directory = fixture();
        let bundle = store(&directory)
            .stage_bytes(&data, &release(&data))
            .unwrap();
        bundle.files.keys().cloned().collect::<Vec<_>>()
    };
    assert_eq!(names.len(), 5);
    for name in names {
        let directory = fixture();
        let store = store(&directory);
        let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
        fs::write(bundle.directory.join(&name), b"modified synthetic fixture").unwrap();
        rejected(store.activate(&bundle), "字典包內容已變動");
        assert!(store.active().unwrap().is_none(), "{name}");
    }
    let directory = fixture();
    let store = store(&directory);
    let mut bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    let path = bundle.directory.join(Mode::ZhHant.config());
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["rules"][0][1] = json!("篡改");
    let modified = serde_json::to_vec(&config).unwrap();
    fs::write(&path, &modified).unwrap();
    bundle
        .files
        .insert(Mode::ZhHant.config().to_owned(), hash(&modified));
    rejected(
        store.activate(&bundle),
        "模式規則與已驗證 MediaWiki 來源不一致",
    );
}
#[test]
fn activation_and_active_state_reject_paths_outside_store() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let outside = fixture();
    let mut bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    bundle.directory = outside.to_path_buf();
    rejected(store.activate(&bundle), "字典路徑超出工具目錄");
    fs::write(
        store.root.join("active.json"),
        serde_json::to_vec(&json!({
            "schema":1, "engine_version":ENGINE_VERSION, "bundle":bundle,
        }))
        .unwrap(),
    )
    .unwrap();
    rejected(store.active(), "字典路徑超出工具目錄");
}
#[test]
fn metadata_chooses_latest_stable_unyanked_and_requires_official_checksum() {
    let item = |number: &str, yanked: bool| {
        json!({
            "num":number, "yanked":yanked, "checksum":"a".repeat(64), "crate_size":100,
            "dl_path":format!("/api/v1/crates/zhconv/{number}/download"), "created_at":"synthetic",
        })
    };
    let parse = |metadata: &serde_json::Value| {
        updater::release_from_metadata(&serde_json::to_vec(metadata).unwrap())
    };
    let base = json!({"crate":{"name":"zhconv"}, "versions":[
        item("0.4.2", false), item("0.4.3-beta", false), item("0.5.0", true), item("0.4.3", false),
    ]});
    // Index of "0.4.3", the release chosen from `base`.
    const LATEST: usize = 3;
    let release = parse(&base).unwrap();
    assert_eq!(release.version, "0.4.3");
    let mut metadata = base.clone();
    metadata["versions"][LATEST]["checksum"] = serde_json::Value::Null;
    rejected(parse(&metadata), "官方 SHA-256 缺失");
    metadata["crate"]["name"] = json!("unofficial");
    rejected(parse(&metadata), "不是官方指定套件的版本資訊");

    metadata = base.clone();
    metadata["versions"][LATEST]["checksum"] = json!("A".repeat(64));
    assert_eq!(parse(&metadata).unwrap().digest, "a".repeat(64));
    let mut uppercase = release.clone();
    uppercase.digest = "A".repeat(64);
    rejected(
        updater::validate_release(&uppercase),
        "官方 SHA-256 資訊缺失或格式不符",
    );

    metadata = base.clone();
    metadata["versions"][LATEST]["dl_path"] = json!("/api/v1/crates/another/0.4.3/download");
    rejected(parse(&metadata), "官方下載路徑不符");

    metadata = base.clone();
    metadata["versions"][0]
        .as_object_mut()
        .unwrap()
        .remove("yanked");
    rejected(parse(&metadata), "官方版本撤回狀態缺失");

    metadata = base.clone();
    metadata["versions"]
        .as_array_mut()
        .unwrap()
        .push(item("0.4.2", true));
    rejected(parse(&metadata), "官方版本清單含有重複版本");

    metadata = base.clone();
    metadata["versions"][LATEST]["crate_size"] = json!(updater::MAX_DOWNLOAD_BYTES + 1);
    rejected(parse(&metadata), "官方字典包大小不支援");
}
#[test]
fn available_compares_version_then_digest_with_current_table() {
    let directory = fixture();
    let store = store(&directory);
    assert_eq!(store.current().unwrap(), embedded());
    let candidate = |version: &str, digest: &str| Release {
        version: version.to_owned(),
        tag: format!("zhconv-{version}"),
        url: format!("https://static.crates.io/crates/zhconv/zhconv-{version}.crate"),
        digest: digest.to_owned(),
        size: 100,
        published: String::new(),
    };
    let parts = ENGINE_VERSION
        .split('.')
        .map(|part| part.parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert!(parts[2] > 0, "older-version case needs a non-zero patch");
    let newer = format!("{}.{}.{}", parts[0], parts[1], parts[2] + 1);
    let older = format!("{}.{}.{}", parts[0], parts[1], parts[2] - 1);
    let other = "b".repeat(64);
    let current = updater::EMBEDDED_CRATE_SHA256;
    assert!(updater::available(&store, &candidate(&newer, current)).unwrap());
    assert!(updater::available(&store, &candidate(ENGINE_VERSION, &other)).unwrap());
    assert!(!updater::available(&store, &candidate(ENGINE_VERSION, current)).unwrap());
    assert!(!updater::available(&store, &candidate(&older, &other)).unwrap());
    rejected(
        updater::available(&store, &candidate("0.4.3-beta", &other)),
        "正式版本格式不支援",
    );
}
#[test]
fn redirects_stay_on_https_crates_io_and_only_success_statuses_pass() {
    let url = |value: &str| reqwest::Url::parse(value).unwrap();
    assert!(updater::redirect_allowed(
        &url("https://static.crates.io/crates/zhconv/zhconv-0.4.2.crate"),
        1
    ));
    assert!(updater::redirect_allowed(
        &url("https://crates.io/api/v1/crates/zhconv"),
        updater::MAX_REDIRECTS - 1
    ));
    assert!(!updater::redirect_allowed(
        &url("https://crates.io/api/v1/crates/zhconv"),
        updater::MAX_REDIRECTS
    ));
    for target in [
        "https://evilcrates.io/crates/zhconv",
        "https://static.crates.io.evil.test/crates/zhconv",
        "http://crates.io/api/v1/crates/zhconv",
        "http://static.crates.io/crates/zhconv/zhconv-0.4.2.crate",
    ] {
        assert!(!updater::redirect_allowed(&url(target), 1), "{target}");
    }
    updater::ensure_success_status(reqwest::StatusCode::OK, None).unwrap();
    let blocked = "https://proxy.example.test/blocked";
    rejected(
        updater::ensure_success_status(reqwest::StatusCode::FOUND, Some(blocked)),
        "302",
    );
    rejected(
        updater::ensure_success_status(reqwest::StatusCode::FOUND, Some(blocked)),
        blocked,
    );
    rejected(
        updater::ensure_success_status(reqwest::StatusCode::NOT_FOUND, None),
        "404",
    );
}
#[test]
fn cancellation_stops_before_network_and_preserves_current_state() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    rejected(
        updater::download_and_stage(&store, &release(&data), &AtomicBool::new(true), &|_| {}),
        "已停止",
    );
    assert!(store.active().unwrap().is_none());
    assert!(
        events(&store)
            .iter()
            .any(|event| event == "download_failed")
    );
}
#[test]
fn old_opencc_mode_names_are_not_accepted_as_mediawiki_modes() {
    for old in ["s2tw", "s2tw.json", "s2twp", "s2twp.json"] {
        rejected(Mode::parse(old), "轉換模式不支援");
    }
    assert_eq!(Mode::parse("zh-Hant.json").unwrap(), Mode::ZhHant);
    assert_eq!(Mode::parse("zh-TW").unwrap(), Mode::ZhTw);
}
#[test]
fn an_unfinished_stage_is_preserved_and_does_not_block_retry() {
    let directory = fixture();
    let store = store(&directory);
    let unfinished = store.root.join("bundles/pending-interrupted-fixture");
    fs::create_dir_all(&unfinished).unwrap();
    fs::write(
        unfinished.join("ZhConversion.php"),
        b"partial synthetic data",
    )
    .unwrap();
    let data = bytes();
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    assert_eq!(
        fs::read(unfinished.join("ZhConversion.php")).unwrap(),
        b"partial synthetic data"
    );
    store.activate(&bundle).unwrap();
    assert!(store.active().unwrap().is_some());
}
#[test]
fn a_damaged_or_outdated_staged_copy_is_moved_aside_and_staged_again() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    fs::remove_file(bundle.directory.join(Mode::ZhTw.config())).unwrap();
    let again = store.stage_bytes(&data, &release(&data)).unwrap();
    assert_eq!(again.directory, bundle.directory);
    updater::validate_files(&again, &store.root).unwrap();
    let stale = stale_entries(&store);
    assert_eq!(stale.len(), 1);
    // The damaged copy is kept as it was, never deleted.
    assert!(stale[0].join(Mode::ZhHant.config()).is_file());
    assert!(!stale[0].join(Mode::ZhTw.config()).exists());
    assert!(
        events(&store)
            .iter()
            .any(|event| event == "stage_replaced_stale")
    );

    // A copy produced for another engine version, with hashes recomputed to match it.
    store.activate(&again).unwrap();
    let path = again.directory.join(Mode::ZhHant.config());
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["engine_version"] = json!("0.0.0");
    let outdated = serde_json::to_vec(&config).unwrap();
    fs::write(&path, &outdated).unwrap();
    let mut recorded = again.clone();
    recorded
        .files
        .insert(Mode::ZhHant.config().to_owned(), hash(&outdated));
    rejected(
        updater::validate_files(&recorded, &store.root),
        "模式規則與已驗證 MediaWiki 來源不一致",
    );
    rejected(store.active(), "回復內附轉換表");
    let restaged = store.stage_bytes(&data, &release(&data)).unwrap();
    updater::validate_files(&restaged, &store.root).unwrap();
    assert_eq!(stale_entries(&store).len(), 2);
    assert_eq!(store.active().unwrap().unwrap().digest, again.digest);

    // A plain file occupying the bundle path also makes way.
    let other = fixture();
    let blocked = Store {
        root: other.to_path_buf(),
    };
    let occupied = blocked
        .root
        .join("bundles")
        .join(bundle.directory.file_name().unwrap());
    fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    fs::write(&occupied, b"synthetic placeholder").unwrap();
    let staged = blocked.stage_bytes(&data, &release(&data)).unwrap();
    assert!(staged.directory.is_dir());
    let stale = stale_entries(&blocked);
    assert_eq!(stale.len(), 1);
    assert_eq!(fs::read(&stale[0]).unwrap(), b"synthetic placeholder");
}
#[test]
fn a_junction_at_the_staged_bundle_path_is_never_followed_or_moved() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let target = directory.join("synthetic-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), b"synthetic").unwrap();
    let bundles = store.root.join("bundles");
    fs::create_dir(&bundles).unwrap();
    let name = format!("{ENGINE_VERSION}-{}", hash(&data));
    junction(&bundles.join(&name), &target);
    rejected(store.stage_bytes(&data, &release(&data)), "無法移到");
    assert!(bundles.join(&name).exists());
    assert!(stale_entries(&store).is_empty());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
}
#[test]
fn embedded_state_from_another_engine_version_falls_back_to_the_embedded_table() {
    let directory = fixture();
    let store = store(&directory);
    let state = store.root.join("active.json");
    fs::write(
        &state,
        serde_json::to_vec(&json!({"schema":1,"engine_version":"0.0.0","bundle":null})).unwrap(),
    )
    .unwrap();
    assert!(store.active().unwrap().is_none());
    assert_eq!(store.current().unwrap(), embedded());

    let data = bytes();
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    fs::write(
        &state,
        serde_json::to_vec(&json!({"schema":1,"engine_version":"0.0.0","bundle":bundle})).unwrap(),
    )
    .unwrap();
    rejected(store.active(), "是為 zhconv 0.0.0 建立");
    rejected(store.active(), "回復內附轉換表");
    store.reset_embedded().unwrap();
    assert_eq!(store.current().unwrap(), embedded());
}
#[test]
fn a_damaged_active_bundle_points_to_the_offline_reset() {
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    store.activate(&bundle).unwrap();
    fs::remove_file(bundle.directory.join("official.crate")).unwrap();
    rejected(store.active(), "啟用中的字典包驗證失敗");
    rejected(store.current(), "回復內附轉換表");
    store.reset_embedded().unwrap();
    assert_eq!(store.current().unwrap(), embedded());
}
#[test]
fn an_unwritable_update_log_stops_activation_and_reset_before_state_changes() {
    fn read_only(path: &Path, value: bool) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(value);
        fs::set_permissions(path, permissions).unwrap();
    }
    let data = bytes();
    let directory = fixture();
    let store = store(&directory);
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    let log = store.root.join("updates.jsonl");
    read_only(&log, true);
    rejected(store.activate(&bundle), "updates.jsonl");
    assert!(store.active().unwrap().is_none());
    assert!(!store.root.join("active.json").exists());
    read_only(&log, false);
    store.activate(&bundle).unwrap();
    read_only(&log, true);
    rejected(store.reset_embedded(), "updates.jsonl");
    assert_eq!(store.active().unwrap().unwrap().digest, bundle.digest);
    read_only(&log, false);
}
#[test]
fn a_store_below_a_relocated_local_appdata_junction_is_usable() {
    let outer = fixture();
    let target = outer.join("target-local");
    fs::create_dir(&target).unwrap();
    let linked = outer.join("linked-local");
    junction(&linked, &target);
    let store = Store {
        root: linked.join("SC2TC-Renamer/mediawiki-dictionaries"),
    };
    assert!(store.active().unwrap().is_none());
    assert_eq!(store.current().unwrap(), embedded());
    let data = bytes();
    let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    store.activate(&bundle).unwrap();
    assert_eq!(store.active().unwrap().unwrap().digest, bundle.digest);
    assert!(
        target
            .join("SC2TC-Renamer/mediawiki-dictionaries/active.json")
            .is_file()
    );
    store.reset_embedded().unwrap();
    assert!(store.active().unwrap().is_none());

    // Links at or below the store root are still refused.
    let guarded = Store {
        root: outer.join("guarded-store"),
    };
    fs::create_dir(&guarded.root).unwrap();
    junction(&guarded.root.join("bundles"), &target);
    rejected(
        guarded.stage_bytes(&data, &release(&data)),
        "字典儲存路徑包含連結或非資料夾項目",
    );
}
#[test]
fn junction_store_and_broken_state_links_are_rejected() {
    let outer = fixture();
    let target = outer.join("target");
    fs::create_dir(&target).unwrap();
    let linked = outer.join("linked-store");
    junction(&linked, &target);
    let store = Store { root: linked };
    rejected(store.active(), "字典儲存路徑包含連結或非資料夾項目");
    rejected(store.reset_embedded(), "字典儲存路徑包含連結或非資料夾項目");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    let store = Store {
        root: outer.join("physical-store"),
    };
    fs::create_dir(&store.root).unwrap();
    junction(&store.root.join("active.json"), &target);
    fs::rename(&target, outer.join("moved-synthetic-target")).unwrap();
    rejected(store.active(), "字典檔案不是實體檔案或超出上限");
    rejected(store.reset_embedded(), "字典檔案不是實體檔案或超出上限");
}
#[test]
fn fixtures_are_removed_after_success_and_kept_after_a_panic() {
    let removed = {
        let directory = fixture();
        fs::write(directory.join("synthetic.txt"), b"synthetic").unwrap();
        directory.to_path_buf()
    };
    assert!(!removed.exists());
    let mut kept = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let directory = fixture();
        kept = Some(directory.to_path_buf());
        panic!("刻意觸發的測試 panic，用來確認失敗夾具會保留");
    }));
    assert!(outcome.is_err());
    let kept = kept.unwrap();
    assert!(kept.is_dir());
    fs::remove_dir_all(&kept).unwrap();
}
