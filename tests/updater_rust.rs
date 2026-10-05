use flate2::{Compression, write::GzEncoder};
use sc2tc_renamer::{
    converter::{self, Converter, ENGINE_VERSION, Mode},
    updater::{self, Release, Store},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};
use tar::{Builder, EntryType, Header};

fn fixture() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/updater-tests")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root).unwrap();
    root
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
    let store = Store { root: fixture() };
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
    let logs = fs::read_to_string(store.root.join("updates.jsonl")).unwrap();
    for event in ["staged", "staged_reused", "activated", "reset_embedded"] {
        assert!(logs.contains(event));
    }
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
    let store = Store { root: fixture() };
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
        assert!(external.convert("bad\0name").is_err());
    }
    let tw = Converter::from_config(&bundle.directory.join(Mode::ZhTw.config())).unwrap();
    assert_eq!(tw.convert("软件 数据库 鼠标").unwrap(), "軟體 資料庫 滑鼠");
}
#[test]
fn bad_hash_size_unofficial_and_prerelease_sources_are_rejected() {
    let data = bytes();
    let store = Store { root: fixture() };
    let mut candidate = release(&data);
    candidate.digest = "0".repeat(64);
    assert!(store.stage_bytes(&data, &candidate).is_err());
    assert!(store.active().unwrap().is_none());
    candidate = release(&data);
    candidate.size += 1;
    assert!(store.stage_bytes(&data, &candidate).is_err());
    for url in [
        "https://example.com/data.crate",
        "http://static.crates.io/crates/zhconv/zhconv-0.4.2.crate",
        "https://static.crates.io.evil.test/crates/zhconv/zhconv-0.4.2.crate",
        "https://static.crates.io/crates/another/another-0.4.2.crate",
    ] {
        candidate = release(&data);
        candidate.url = url.to_owned();
        assert!(updater::validate_release(&candidate).is_err());
    }
    for value in ["0.4.3-beta", "v0.4.2", "00.4.2", "../0.4.2", "0.4"] {
        candidate = release(&data);
        candidate.version = value.to_owned();
        assert!(updater::validate_release(&candidate).is_err());
    }
    assert!(
        fs::read_to_string(store.root.join("updates.jsonl"))
            .unwrap()
            .contains("stage_failed")
    );
}
#[test]
fn unsupported_empty_malformed_duplicate_and_executable_php_are_rejected() {
    let source = std::str::from_utf8(converter::EMBEDDED_SOURCE).unwrap();
    let examples = [
        Vec::new(), b"not PHP".to_vec(), vec![0xff],
        source.replacen("'㐷' => '傌',", "'㐷' => '傌',\n'㐷' => '另一值',", 1).into_bytes(),
        source.replacen("ZH_TO_HANT", "UNKNOWN_TABLE", 1).into_bytes(),
        source.replacen("public const", "publicconst", 1).into_bytes(),
        source.replacen("namespace MediaWiki", "namespaceMediaWiki", 1).into_bytes(),
        source.replacen("class ZhConversion {", "class ZhConversion { public const ZH_TO_HANT = ['合成' => '資料'];", 1).into_bytes(),
        source.replacen("'㐷' => '傌',", "'㐷' => call_user_func('bad'),", 1).into_bytes(),
        source.replacen("'㐷' => '傌',", "'' => '傌',", 1).into_bytes(),
        source.replacen("'㐷' => '傌',", "'㐷' => '',", 1).into_bytes(),
        format!("{source}\neval('malicious');").into_bytes(),
        source.replace("];", "]").into_bytes(),
        b"<?php namespace MediaWiki\\Languages\\Data; class ZhConversion { public const ZH_TO_HANT = []; }".to_vec(),
    ];
    let store = Store { root: fixture() };
    for source in examples {
        assert!(converter::parse_mediawiki(&source).is_err());
        let data = archive(&[(
            "zhconv-0.4.2/data/ZhConversion.php",
            &source,
            EntryType::Regular,
        )]);
        assert!(store.stage_bytes(&data, &release(&data)).is_err());
    }
    assert!(store.active().unwrap().is_none());
}
#[test]
fn archive_traversal_links_duplicates_and_missing_table_are_rejected() {
    let source = converter::EMBEDDED_SOURCE;
    let cases = [
        archive(&[("../outside.php", source, EntryType::Regular)]),
        archive(&[(
            "zhconv-0.4.2/data/../ZhConversion.php",
            source,
            EntryType::Regular,
        )]),
        archive(&[(
            "zhconv-0.4.2/data\\ZhConversion.php",
            source,
            EntryType::Regular,
        )]),
        archive(&[(
            "zhconv-0.4.2/data/ZhConversion.php",
            b"",
            EntryType::Symlink,
        )]),
        archive(&[("zhconv-0.4.2/data/ZhConversion.php", b"", EntryType::Link)]),
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
        archive(&[(
            "another-0.4.2/data/ZhConversion.php",
            source,
            EntryType::Regular,
        )]),
        archive(&[("zhconv-0.4.2/README.md", b"synthetic", EntryType::Regular)]),
        b"bad gzip".to_vec(),
    ];
    let store = Store { root: fixture() };
    for data in cases {
        assert!(store.stage_bytes(&data, &release(&data)).is_err());
    }
    assert!(store.active().unwrap().is_none());
}
#[test]
fn changed_staged_files_or_recomputed_rules_cannot_be_activated() {
    let data = bytes();
    for name in [
        "ZhConversion.php",
        "zh-Hant.json",
        "official.crate",
        "release.json",
    ] {
        let store = Store { root: fixture() };
        let bundle = store.stage_bytes(&data, &release(&data)).unwrap();
        fs::write(bundle.directory.join(name), b"modified synthetic fixture").unwrap();
        assert!(store.activate(&bundle).is_err());
        assert!(store.active().unwrap().is_none());
    }
    let store = Store { root: fixture() };
    let mut bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    let path = bundle.directory.join(Mode::ZhHant.config());
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["rules"][0][1] = json!("篡改");
    let modified = serde_json::to_vec(&config).unwrap();
    fs::write(&path, &modified).unwrap();
    bundle
        .files
        .insert(Mode::ZhHant.config().to_owned(), hash(&modified));
    assert!(store.activate(&bundle).is_err());
}
#[test]
fn activation_and_active_state_reject_paths_outside_store() {
    let data = bytes();
    let store = Store { root: fixture() };
    let mut bundle = store.stage_bytes(&data, &release(&data)).unwrap();
    bundle.directory = fixture();
    assert!(store.activate(&bundle).is_err());
    fs::write(
        store.root.join("active.json"),
        serde_json::to_vec(&json!({
            "schema":1, "engine_version":ENGINE_VERSION, "bundle":bundle,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(store.active().is_err());
}
#[test]
fn metadata_chooses_latest_stable_unyanked_and_requires_official_checksum() {
    let item = |number: &str, yanked: bool| {
        json!({
            "num":number, "yanked":yanked, "checksum":"a".repeat(64), "crate_size":100,
            "dl_path":format!("/api/v1/crates/zhconv/{number}/download"), "created_at":"synthetic",
        })
    };
    let mut metadata = json!({"crate":{"name":"zhconv"}, "versions":[
        item("0.4.2", false), item("0.4.3-beta", false), item("0.5.0", true), item("0.4.3", false),
    ]});
    let release = updater::release_from_metadata(&serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert_eq!(release.version, "0.4.3");
    metadata["versions"][3]["checksum"] = serde_json::Value::Null;
    assert!(updater::release_from_metadata(&serde_json::to_vec(&metadata).unwrap()).is_err());
    metadata["crate"]["name"] = json!("unofficial");
    assert!(updater::release_from_metadata(&serde_json::to_vec(&metadata).unwrap()).is_err());
}
#[test]
fn cancellation_stops_before_network_and_preserves_current_state() {
    let data = bytes();
    let store = Store { root: fixture() };
    assert!(
        updater::download_and_stage(&store, &release(&data), &AtomicBool::new(true), &|_| {})
            .is_err()
    );
    assert!(store.active().unwrap().is_none());
    assert!(
        fs::read_to_string(store.root.join("updates.jsonl"))
            .unwrap()
            .contains("download_failed")
    );
}
#[test]
fn old_opencc_mode_names_are_not_accepted_as_mediawiki_modes() {
    for old in ["s2tw", "s2tw.json", "s2twp", "s2twp.json"] {
        assert!(Mode::parse(old).is_err());
    }
    assert_eq!(Mode::parse("zh-Hant.json").unwrap(), Mode::ZhHant);
    assert_eq!(Mode::parse("zh-TW").unwrap(), Mode::ZhTw);
}
#[test]
fn an_unfinished_stage_is_preserved_and_does_not_block_retry() {
    let store = Store { root: fixture() };
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
fn junction_store_and_broken_state_links_are_rejected() {
    use std::{os::windows::process::CommandExt, process::Command};
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    fn junction(link: &std::path::Path, target: &std::path::Path) {
        let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "''");
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
    let outer = fixture();
    let target = outer.join("target");
    fs::create_dir(&target).unwrap();
    let linked = outer.join("linked-store");
    junction(&linked, &target);
    let store = Store { root: linked };
    assert!(store.active().is_err());
    assert!(store.reset_embedded().is_err());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    let store = Store {
        root: outer.join("physical-store"),
    };
    fs::create_dir(&store.root).unwrap();
    junction(&store.root.join("active.json"), &target);
    fs::rename(&target, outer.join("moved-synthetic-target")).unwrap();
    assert!(store.active().is_err());
    assert!(store.reset_embedded().is_err());
}
