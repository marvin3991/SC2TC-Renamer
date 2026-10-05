use sc2tc_renamer::{
    converter::Converter,
    engine::{self, Status},
    journal::{self, Journal},
    native,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

fn fixture() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/rust-tests")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&path).unwrap();
    path
}
fn file(root: &std::path::Path, name: &str) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"synthetic document bytes\0\xff").unwrap();
    path
}
fn snapshot(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut output = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path)
            } else {
                output.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    output.sort();
    output
}
fn plan(root: &std::path::Path) -> engine::Plan {
    engine::make_plan(&[root.to_owned()], &AtomicBool::new(false), &|_| {}).unwrap()
}

#[test]
fn mediawiki_traditional_conversion() {
    let converter = Converter::new().unwrap();
    assert_eq!(
        converter.convert("软件 数据库 文件夹 头发 发展").unwrap(),
        "軟件 數據庫 文件夾 頭髮 發展"
    );
    assert_eq!(converter.convert("").unwrap(), "");
    assert_eq!(
        converter.convert("English_123.pdf").unwrap(),
        "English_123.pdf"
    );
}

#[test]
fn branded_storage_is_separate_and_both_history_locations_are_protected() {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
    let history = local.join("SC2TC-Renamer/history");
    let legacy_history = local.join("OpenCCRenamer/history");
    assert_eq!(engine::history_root().unwrap(), history);
    assert_eq!(
        sc2tc_renamer::updater::Store::standard().unwrap().root,
        local.join("SC2TC-Renamer/mediawiki-dictionaries")
    );
    let plan = plan(&fixture());
    for protected in [history, legacy_history] {
        assert!(plan.exclusions.contains(&native::text(&protected).unwrap()));
    }
}

#[test]
fn operation_lock_remains_compatible_with_previous_versions() {
    use std::{sync::mpsc, time::Duration};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let previous_version = std::thread::spawn(move || {
        struct PreviousMutex {
            handle: HANDLE,
            acquired: bool,
        }
        impl Drop for PreviousMutex {
            fn drop(&mut self) {
                unsafe {
                    if self.acquired {
                        ReleaseMutex(self.handle);
                    }
                    CloseHandle(self.handle);
                }
            }
        }
        let name = native::wide(std::ffi::OsStr::new("Local\\OpenCCRenamerOperationsV1"));
        let mut mutex = PreviousMutex {
            handle: unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) },
            acquired: false,
        };
        assert!(!mutex.handle.is_null());
        let status = unsafe { WaitForSingleObject(mutex.handle, 0) };
        assert!(status == WAIT_OBJECT_0 || status == WAIT_ABANDONED);
        mutex.acquired = true;
        acquired_tx.send(()).unwrap();
        let signalled = release_rx.recv_timeout(HANDSHAKE_TIMEOUT).is_ok();
        assert!(signalled);
    });
    acquired_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
    let blocked = native::OperationLock::acquire().is_err();
    release_tx.send(()).unwrap();
    previous_version.join().unwrap();
    assert!(
        blocked,
        "The new application must share the previous-version operation lock."
    );
    assert!(native::OperationLock::acquire().is_ok());
}

#[test]
fn mediawiki_names_mixed_scripts_and_second_pass_remain_stable() {
    use sc2tc_renamer::converter::Mode;
    let names = [
        ("岳飞.txt", "岳飛.txt"),
        ("岳飛.txt", "岳飛.txt"),
        ("岳阳", "岳陽"),
        ("于谦", "于謙"),
        ("于謙", "于謙"),
        ("郁达夫", "郁達夫"),
        ("干将", "干將"),
        ("头发", "頭髮"),
        ("散发传单", "散發傳單"),
        ("😀岳飞-ABC.txt", "😀岳飛-ABC.txt"),
    ];
    for mode in [Mode::ZhHant, Mode::ZhTw] {
        let converter = Converter::with_mode(mode).unwrap();
        for (original, expected) in names {
            let once = converter.convert(original).unwrap();
            assert_eq!(once, expected, "{mode:?}: {original}");
            assert_eq!(converter.convert(&once).unwrap(), once);
        }
        assert!(converter.convert("invalid\0name").is_err());
        // Filenames are plain text, not executable MediaWiki markup.
        assert_eq!(converter.convert("-{A|ABC}-").unwrap(), "-{A|ABC}-");
    }
    assert!(Mode::parse("s2tw.json").is_err());
    assert!(Mode::parse("s2twp.json").is_err());
}

#[test]
fn renamed_person_names_are_unchanged_on_rescan_and_restore_exactly() {
    use sc2tc_renamer::converter::Mode;
    for (mode, mixed_target) in [
        (Mode::ZhHant, "岳飛的軟件資料.TXT"),
        (Mode::ZhTw, "岳飛的軟體資料.TXT"),
    ] {
        let base = fixture();
        let root = base.join("names");
        fs::create_dir(&root).unwrap();
        file(&root, "人物/岳飛.txt");
        file(&root, "人物/岳飞传.txt");
        file(&root, "人物/于谦资料.txt");
        file(&root, "混合/岳飛的软件资料.TXT");
        let before = snapshot(&root);
        let plan = engine::make_plan_with_mode(
            std::slice::from_ref(&root),
            mode,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        let journal = Journal::create(&base.join("history"), &plan).unwrap();
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(root.join("人物/岳飛.txt").exists());
        assert!(root.join("人物/岳飛傳.txt").exists());
        assert!(root.join("人物/于謙資料.txt").exists());
        assert!(root.join("混合").join(mixed_target).exists());
        let rescan = engine::make_plan_with_mode(
            std::slice::from_ref(&root),
            mode,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        assert!(
            rescan
                .rows
                .iter()
                .all(|row| row.status == Status::Unchanged)
        );
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(snapshot(&root), before);
    }
}

#[test]
fn old_schema_two_previews_only_allow_recovery() {
    for mode in ["s2tw.json", "s2twp.json"] {
        let base = fixture();
        let root = base.join("old-history");
        fs::create_dir(&root).unwrap();
        let source = file(&root, "报告.txt");
        let before = snapshot(&root);
        let mut old_plan = plan(&root);
        old_plan.mode = mode.to_owned();
        old_plan.dictionary_version = "1.4.2".to_owned();
        old_plan.dictionary_hash = "legacy-dictionary".to_owned();
        let journal = Journal::create(&base.join("history"), &old_plan).unwrap();
        assert_eq!(journal.load().unwrap().mode, mode);
        let error =
            journal::apply(&old_plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert!(error.to_string().contains("只能復原"));
        assert_eq!(snapshot(&root), before);
        assert!(
            journal
                .events()
                .unwrap()
                .iter()
                .all(|event| event["event"] != "apply_start")
        );
        // Reproduce a completed legacy rename without loading the former engine.
        let row = &old_plan.rows[0];
        journal.append("apply_start", json!({})).unwrap();
        journal
            .append("rename_intent", json!({"id":row.id}))
            .unwrap();
        native::rename_no_replace(&source, &source.with_file_name(&row.new), &row.identity)
            .unwrap();
        journal.append("rename_done", json!({"id":row.id})).unwrap();
        journal
            .append("apply_end", json!({"status":"complete"}))
            .unwrap();
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(snapshot(&root), before);
    }
}
#[test]
fn selectable_taiwan_mode_changes_terms_and_persists_mode() {
    let converter = Converter::with_mode(sc2tc_renamer::converter::Mode::ZhTw).unwrap();
    assert_eq!(
        converter.convert("软件 数据库 鼠标").unwrap(),
        "軟體 資料庫 滑鼠"
    );
    let base = fixture();
    file(&base, "软件说明.txt");
    let plan = engine::make_plan_with_mode(
        &[base],
        sc2tc_renamer::converter::Mode::ZhTw,
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(plan.mode, "zh-TW.json");
    assert_eq!(plan.rows[0].new, "軟體說明.txt");
    let journal = Journal::create(&fixture(), &plan).unwrap();
    assert_eq!(journal.load().unwrap().mode, "zh-TW.json");
}
#[test]
fn actual_nested_rename_and_recovery() {
    let base = fixture();
    let root = base.join("文件");
    fs::create_dir(&root).unwrap();
    file(&root, "简体目录/第二层文件夹/软件资料.docx");
    file(&root, "数据备份.tar.gz");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let count = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert!(root.join("簡體目錄/第二層文件夾/軟件資料.docx").exists());
    assert!(root.join("數據備份.tar.gz").exists());
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        count
    );
    assert_eq!(snapshot(&root), before);
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
    assert!(journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).is_err());
}
#[test]
fn collision_files_never_replaced() {
    let base = fixture();
    let root = base.join("files");
    fs::create_dir(&root).unwrap();
    file(&root, "报告.txt");
    fs::write(root.join("報告.txt"), b"traditional original").unwrap();
    file(&root, "头发.jpg");
    file(&root, "头髮.jpg");
    let before = snapshot(&root);
    let plan = plan(&root);
    assert_eq!(
        plan.rows
            .iter()
            .filter(|r| r.status == Status::Conflict)
            .count(),
        3
    );
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    assert_eq!(
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
    assert_eq!(snapshot(&root), before);
}
#[test]
fn selected_file_only_does_not_convert_siblings() {
    let base = fixture();
    let source = file(&base, "报告.txt");
    let sibling = file(&base, "软件.txt");
    let plan = engine::make_plan(&[source], &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(plan.rows.len(), 1);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert!(base.join("報告.txt").exists());
    assert!(sibling.exists());
    journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
}
#[test]
fn preview_change_stops_before_any_rename() {
    let base = fixture();
    let source = file(&base, "报告.txt");
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    fs::write(&source, b"changed after preview").unwrap();
    assert!(journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).is_err());
    assert!(source.exists());
    assert!(
        !journal
            .events()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "apply_start")
    );
}
#[test]
fn target_created_late_is_not_overwritten() {
    let base = fixture();
    let source = file(&base, "报告.txt");
    let identity = native::metadata(&source).unwrap().identity;
    let target = file(&base, "報告.txt");
    fs::write(&target, b"do not replace").unwrap();
    assert!(native::rename_no_replace(&source, &target, &identity).is_err());
    assert_eq!(fs::read(target).unwrap(), b"do not replace");
    assert!(source.exists());
}
#[test]
fn rename_buffer_boundaries_preserve_exact_unicode_names() {
    let base = fixture();
    const ALIGNMENT_CASES: usize = 8;
    for count in 0..ALIGNMENT_CASES {
        let source = file(&base, &format!("来源{}.txt", "甲".repeat(count)));
        let target = source.with_file_name(format!("精確名稱{}.txt", "乙".repeat(count)));
        let id = native::metadata(&source).unwrap().identity;
        native::rename_no_replace(&source, &target, &id).unwrap();
        assert!(target.exists());
        let matched = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name() == target.file_name().unwrap());
        assert!(matched, "exact target name missing");
    }
    assert_eq!(fs::read_dir(&base).unwrap().count(), ALIGNMENT_CASES);
}
#[test]
fn intent_without_done_is_reconciled_and_restored() {
    let base = fixture();
    let source = file(&base, "报告.txt");
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let row = plan
        .rows
        .iter()
        .find(|r| r.status == Status::Ready)
        .unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":row.id}))
        .unwrap();
    native::rename_no_replace(&source, &source.with_file_name(&row.new), &row.identity).unwrap();
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        1
    );
    assert!(source.exists());
}
#[test]
fn cancel_after_first_operation_remains_recoverable() {
    let base = fixture();
    file(&base, "报告.txt");
    file(&base, "软件.txt");
    let before = snapshot(&base);
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let cancel = AtomicBool::new(false);
    let mut_seen = std::sync::atomic::AtomicUsize::new(0);
    let progress = |message: String| {
        if message.starts_with("改名中") && mut_seen.fetch_add(1, Ordering::Relaxed) == 0 {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    assert!(journal::apply(&plan, &journal, &cancel, &progress).is_err());
    journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(snapshot(&base), before);
}
#[test]
fn backup_edit_and_invalid_names_stop_safely() {
    for name in ["", "CON.txt", "LPT¹.txt", "..", "bad/child", "bad.", "bad "] {
        assert!(!native::valid_name(name));
    }
    let base = fixture();
    file(&base, "报告.txt");
    let mut plan = plan(&base);
    plan.rows[0].new = "..\\outside.txt".to_owned();
    assert!(engine::validate(&plan).is_err());
}
#[test]
fn duplicate_and_overlapping_scopes() {
    let base = fixture();
    let child = base.join("child");
    fs::create_dir(&child).unwrap();
    assert_eq!(
        engine::normalise(&[base.clone(), base.clone()])
            .unwrap()
            .len(),
        1
    );
    assert!(engine::normalise(&[base, child]).is_err());
    assert!(engine::normalise(&[]).is_err());
}
#[test]
fn changed_original_name_blocks_undo() {
    let base = fixture();
    file(&base, "报告.txt");
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    file(&base, "报告.txt");
    let before = snapshot(&base);
    assert!(journal::undo(&journal, &AtomicBool::new(false), &|_| {}).is_err());
    assert_eq!(snapshot(&base), before);
}
#[test]
fn empty_preview_and_csv_are_valid() {
    let base = fixture();
    let plan = plan(&base);
    assert!(plan.rows.is_empty());
    let csv = base.join("preview.csv");
    engine::save_csv(&plan, &csv).unwrap();
    assert!(fs::read(csv).unwrap().starts_with(&[0xef, 0xbb, 0xbf]));
    engine::verify(&plan, &BTreeSet::new(), &AtomicBool::new(false), &|_| {}).unwrap_err(); // Exporting into the scope invalidates its snapshot.
}

#[test]
fn changed_scan_issues_report_path_code_and_reason_without_relaxing_checks() {
    let base = fixture();
    file(&base, "报告.txt");
    let mut plan = plan(&base);
    plan.issues.push(engine::Issue {
        path: native::text(&base).unwrap(),
        code: "synthetic-access-issue".to_owned(),
        reason: "synthetic scan was previously blocked".to_owned(),
    });
    let message = engine::verify(&plan, &BTreeSet::new(), &AtomicBool::new(false), &|_| {})
        .unwrap_err()
        .to_string();
    assert!(message.contains("掃描問題已變更"));
    assert!(message.contains("synthetic-access-issue"));
    assert!(message.contains("synthetic scan was previously blocked"));
    assert!(message.contains(&native::text(&base).unwrap()));
}

#[test]
fn locked_file_stops_without_replacing_then_recovers() {
    use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};
    let base = fixture();
    let source = file(&base, "报告.txt");
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let wide = native::wide(source.as_os_str());
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(handle, INVALID_HANDLE_VALUE);
    let result = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {});
    unsafe {
        CloseHandle(handle);
    }
    assert!(result.is_err());
    assert!(source.exists());
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
}
#[test]
fn hidden_child_protects_parent_but_readable_sibling_converts() {
    use windows_sys::Win32::Storage::FileSystem::*;
    let base = fixture();
    let hidden = file(&base, "简体目录/隐藏.txt");
    file(&base, "简体目录/报告.txt");
    assert_ne!(
        unsafe {
            SetFileAttributesW(
                native::wide(hidden.as_os_str()).as_ptr(),
                FILE_ATTRIBUTE_HIDDEN,
            )
        },
        0
    );
    let plan = plan(&base);
    assert_eq!(
        plan.rows
            .iter()
            .find(|r| r.old == "简体目录")
            .unwrap()
            .status,
        Status::Blocked
    );
    let journal = Journal::create(&fixture(), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert!(hidden.exists());
    assert!(base.join("简体目录/報告.txt").exists());
    journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
}
#[test]
fn interrupted_undo_and_truncated_tail_are_resumable() {
    use std::io::Write;
    let base = fixture();
    file(&base, "简体目录/报告.txt");
    let before = snapshot(&base);
    let plan = plan(&base);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let (_, actions) = journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let action = &actions[0];
    journal
        .append("undo_intent", json!({"id":action.id}))
        .unwrap();
    native::rename_no_replace(
        &action.source,
        &action.target,
        &plan.rows[action.id].identity,
    )
    .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(journal.directory.join("events.jsonl"))
        .unwrap()
        .write_all(b"{\"event\":")
        .unwrap();
    journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(snapshot(&base), before);
    assert!(fs::read_dir(&journal.directory).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("interrupted-tail-")
    }));
}
// Captured schema-1 structure from version 1.1.0. Only the root and native
// metadata are replaced for each synthetic test directory; conversion rules
// and completed-operation ordering do not depend on an installed old runtime.
const LEGACY_SCHEMA_ONE_FIXTURE: &str = r#"{
  "schema": 1,
  "version": "1.1.0",
  "mode": "s2tw.json",
  "created": "2026-10-04T09:42:50.258527+00:00",
  "roots": ["<SYNTHETIC_ROOT>"],
  "records": [
    {"root": 0, "parts": [], "kind": "dir", "identity": [], "protected": ""},
    {"root": 0, "parts": [".git"], "kind": "dir", "identity": [], "protected": "系統或程式資料夾：不處理"},
    {"root": 0, "parts": ["简体目录"], "kind": "dir", "identity": [], "protected": ""},
    {"root": 0, "parts": ["简体目录", "报告.txt"], "kind": "file", "identity": [], "protected": ""}
  ],
  "issues": [],
  "rows": [
    {"root": 0, "parts": [".git"], "kind": "dir", "identity": [], "protected": "系統或程式資料夾：不處理", "id": 0, "old": ".git", "new": ".git", "status": "excluded", "reason": "系統或程式資料夾：不處理"},
    {"root": 0, "parts": ["简体目录"], "kind": "dir", "identity": [], "protected": "", "id": 1, "old": "简体目录", "new": "簡體目錄", "status": "ready", "reason": ""},
    {"root": 0, "parts": ["简体目录", "报告.txt"], "kind": "file", "identity": [], "protected": "", "id": 2, "old": "报告.txt", "new": "報告.txt", "status": "ready", "reason": ""}
  ]
}"#;

fn schema_one_fixture(root: &std::path::Path) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(LEGACY_SCHEMA_ONE_FIXTURE).unwrap();
    value["roots"] = json!([native::text(root).unwrap()]);
    for record in value["records"].as_array_mut().unwrap() {
        let mut path = root.to_owned();
        for part in record["parts"].as_array().unwrap() {
            path.push(part.as_str().unwrap());
        }
        let metadata = native::metadata(&path).unwrap();
        let identity = &metadata.identity;
        let mut fields = vec![json!(identity.volume), json!(identity.file_id)];
        if record["kind"] == "file" {
            let nanos = (i128::from(identity.modified_ticks.unwrap())
                - i128::from(native::WINDOWS_UNIX_EPOCH_TICKS))
                * native::NANOS_PER_FILETIME_TICK;
            fields.extend([json!(identity.size.unwrap()), json!(nanos)]);
        }
        record["identity"] = json!(fields);
    }
    let records = value["records"].as_array().unwrap().clone();
    for row in value["rows"].as_array_mut().unwrap() {
        let record = records.iter().find(|r| r["parts"] == row["parts"]).unwrap();
        row["identity"] = record["identity"].clone();
    }
    value
}

fn schema_one_journal(base: &std::path::Path, value: &serde_json::Value) -> Journal {
    let directory = base.join("schema-one-history");
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("plan.json"),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .unwrap();
    let journal = Journal { directory };
    journal.append("preview_saved", json!({})).unwrap();
    journal
}

#[test]
fn schema_one_history_is_recovered_without_the_old_runtime() {
    let base = fixture();
    let root = base.join("legacy");
    fs::create_dir(&root).unwrap();
    file(&root, "简体目录/报告.txt");
    file(&root, ".git/config");
    let before = snapshot(&root);
    let journal = schema_one_journal(&base, &schema_one_fixture(&root));
    let raw_plan = fs::read(journal.directory.join("plan.json")).unwrap();
    let plan = journal.load().unwrap();
    assert!(plan.legacy);
    assert_eq!(plan.dictionary_version, "舊版紀錄");
    assert!(journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).is_err());
    let mut rows: Vec<_> = plan
        .rows
        .iter()
        .filter(|row| row.status == Status::Ready)
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(std::path::Path::new(&row.path).components().count()));
    journal.append("apply_start", json!({})).unwrap();
    let mut mapping = std::collections::HashMap::new();
    for row in &rows {
        let source = engine::mapped(&row.path, &mapping);
        let target = source.with_file_name(&row.new);
        journal
            .append("rename_intent", json!({"id":row.id}))
            .unwrap();
        native::rename_no_replace(&source, &target, &row.identity).unwrap();
        mapping.insert(
            native::key(std::path::Path::new(&row.path)),
            row.new.clone(),
        );
        journal.append("rename_done", json!({"id":row.id})).unwrap();
    }
    journal
        .append("apply_end", json!({"status":"complete"}))
        .unwrap();
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        rows.len()
    );
    assert_eq!(snapshot(&root), before);
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
    assert_eq!(
        fs::read(journal.directory.join("plan.json")).unwrap(),
        raw_plan
    );
}

#[test]
fn schema_one_large_file_ids_and_nanosecond_timestamps_remain_exact() {
    let base = fixture();
    let root = base.join("legacy");
    fs::create_dir(&root).unwrap();
    let source = file(&root, "简体目录/报告.txt");
    file(&root, ".git/config");
    let mut value = schema_one_fixture(&root);
    const FILE_ID_FIELD: usize = 1; // Schema 1: device, inode, file size, mtime_ns.
    for collection in ["records", "rows"] {
        for record in value[collection]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|r| r["kind"] == "file")
        {
            record["identity"][FILE_ID_FIELD] = json!(u128::MAX);
        }
    }
    let journal = schema_one_journal(&base, &value);
    let loaded = journal.load().unwrap();
    let identity = &loaded
        .records
        .iter()
        .find(|r| r.kind == engine::Kind::File)
        .unwrap()
        .identity;
    assert_eq!(identity.file_id, u128::MAX);
    assert_eq!(
        identity.modified_ticks,
        native::metadata(&source).unwrap().identity.modified_ticks
    );
    assert!(journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).is_err());
}

#[test]
fn schema_one_invalid_identity_or_path_stops_before_recovery() {
    let base = fixture();
    let root = base.join("legacy");
    fs::create_dir(&root).unwrap();
    file(&root, "简体目录/报告.txt");
    file(&root, ".git/config");
    let before = snapshot(&root);
    let value = schema_one_fixture(&root);
    const FILE_ID_FIELD: usize = 1; // Schema-1 inode position.
    for invalid_path in [false, true] {
        let mut broken = value.clone();
        let file_record = broken["records"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["kind"] == "file")
            .unwrap();
        if invalid_path {
            file_record["parts"] = json!(["..", "报告.txt"]);
        } else {
            file_record["identity"][FILE_ID_FIELD] = json!("not-an-integer");
        }
        let journal = schema_one_journal(&fixture(), &broken);
        assert!(journal.load().is_err());
        assert!(journal::undo(&journal, &AtomicBool::new(false), &|_| {}).is_err());
        assert_eq!(snapshot(&root), before);
    }
}
#[test]
fn isolated_volume_root_supports_top_level_and_nested_names() {
    let base = fixture();
    file(&base, "简体目录/报告.txt");
    file(&base, "软件.txt");
    let before = snapshot(&base);
    const LETTERS: &str = "ZYXWVUTSRQPONMLKJIHGF";
    let occupied = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    let letter = LETTERS
        .chars()
        .find(|&c| occupied & (1 << (c as u32 - 'A' as u32)) == 0)
        .unwrap();
    let drive = format!("{letter}:");
    let created = std::process::Command::new("subst.exe")
        .arg(&drive)
        .arg(&base)
        .output()
        .unwrap();
    assert!(created.status.success());
    let volume = PathBuf::from(format!("{drive}\\"));
    let result = std::panic::catch_unwind(|| {
        let plan = plan(&volume);
        let journal = Journal::create(&fixture(), &plan).unwrap();
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(volume.join("簡體目錄/報告.txt").exists());
        assert!(volume.join("軟件.txt").exists());
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(snapshot(&base), before);
    });
    let mut buffer = vec![0u16; native::MAX_PATH_UNITS + 1];
    assert_ne!(
        unsafe {
            windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW(
                native::wide(std::ffi::OsStr::new(&drive)).as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        },
        0
    );
    let target = String::from_utf16_lossy(&buffer[..buffer.iter().position(|c| *c == 0).unwrap()]);
    assert_eq!(
        native::key(std::path::Path::new(target.strip_prefix("\\??\\").unwrap())),
        native::key(&base)
    );
    assert!(
        std::process::Command::new("subst.exe")
            .args([&drive, "/D"])
            .output()
            .unwrap()
            .status
            .success()
    );
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
