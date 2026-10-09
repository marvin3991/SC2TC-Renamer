//! Journal recovery, interruption and identity-overlay tests. Every test works
//! on synthetic data under `work/` and an isolated, empty dictionary store.
use sc2tc_renamer::{
    engine::{self, Identities, Kind, Plan, Row, Status},
    journal::{self, Journal},
    native::{self, Identity},
    updater::Store,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

static ISOLATE_DICTIONARY: Once = Once::new();
/// Other test processes on this machine share the cross-process operation
/// lock, which `apply` and `undo` take without waiting.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// 600 attempts × 100 ms: wait up to one minute for those processes.
const LOCK_RETRY_LIMIT: u32 = 600;

/// Holds the operation lock for a whole test. The mutex is re-entrant for its
/// owning thread, so `apply` and `undo` in the test still acquire it, while
/// other processes cannot interleave their operations with the test steps.
fn exclusive() -> native::OperationLock {
    for _ in 0..LOCK_RETRY_LIMIT {
        match native::OperationLock::acquire() {
            Ok(lock) => return lock,
            Err(error) if error.to_string() == native::OPERATION_BUSY => {
                std::thread::sleep(LOCK_RETRY_INTERVAL)
            }
            Err(error) => panic!("{error:#}"),
        }
    }
    panic!("{}", native::OPERATION_BUSY);
}

fn work() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("work")
}
fn isolate_dictionary() {
    ISOLATE_DICTIONARY.call_once(|| {
        Store::override_standard_root(work().join("journal_rust-dictionary-store")).unwrap();
    });
}
fn fixture() -> PathBuf {
    isolate_dictionary();
    let path = work()
        .join("journal-rust-tests")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&path).unwrap();
    path
}
fn file(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"synthetic document bytes\0\xff").unwrap();
    path
}
/// Relative paths of every folder (`None`) and file (its bytes) below `root`.
fn snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut output = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                output.push((relative, None));
                pending.push(path);
            } else {
                output.push((relative, Some(fs::read(&path).unwrap())));
            }
        }
    }
    output.sort();
    output
}
fn plan(root: &Path) -> Plan {
    engine::make_plan(&[root.to_owned()], &AtomicBool::new(false), &|_| {}).unwrap()
}
fn row<'a>(plan: &'a Plan, old: &str) -> &'a Row {
    plan.rows.iter().find(|r| r.old == old).unwrap()
}
fn ready(plan: &Plan) -> Vec<&Row> {
    plan.rows
        .iter()
        .filter(|r| r.status == Status::Ready)
        .collect()
}
fn identity(path: &Path) -> Identity {
    native::metadata(path).unwrap().identity
}
fn names(journal: &Journal) -> Vec<String> {
    journal
        .events()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect()
}
fn raw_events(journal: &Journal) -> String {
    String::from_utf8(fs::read(journal.directory.join("events.jsonl")).unwrap()).unwrap()
}
fn has_interrupted_tail(journal: &Journal) -> bool {
    fs::read_dir(&journal.directory).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("interrupted-tail-")
    })
}
/// Renames a row as an interrupted apply would, outside `journal::apply`.
fn rename_row(row: &Row) -> PathBuf {
    let source = PathBuf::from(&row.path);
    let target = source.with_file_name(&row.new);
    native::rename_no_replace(&source, &target, &identity(&source)).unwrap();
    target
}
fn undo(journal: &Journal) -> anyhow::Result<usize> {
    journal::undo(journal, &AtomicBool::new(false), &|_| {})
}
fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}
/// Arbitrary bits flipped to give a synthetic, different file ID.
const RENUMBER_BITS: u128 = 0x5a5a_0000;
/// The same item with another file ID, as a FAT volume reports after a rename.
fn renumbered(identity: &Identity) -> Identity {
    Identity {
        file_id: identity.file_id ^ RENUMBER_BITS,
        ..identity.clone()
    }
}
/// Replaces the scan-time identity of one row, as if the scan ran on FAT.
fn with_identity(mut plan: Plan, id: usize, identity: &Identity) -> Plan {
    let path = plan.rows[id].path.clone();
    plan.rows[id].identity = identity.clone();
    for record in plan.records.iter_mut().filter(|r| r.path == path) {
        record.identity = identity.clone();
    }
    plan
}

// F023: the intent_reconciled entry written by one undo is read by the next.
#[test]
fn reconciled_intent_is_replayed_by_the_next_undo() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    file(&root, "软件.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let (first, second) = (row(&plan, "报告.txt"), row(&plan, "软件.txt"));
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":first.id}))
        .unwrap();
    let renamed = rename_row(first);
    journal
        .append(
            "rename_done",
            json!({"id":first.id,"identity":identity(&renamed)}),
        )
        .unwrap();
    journal
        .append("rename_intent", json!({"id":second.id}))
        .unwrap();
    rename_row(second);

    let cancel = AtomicBool::new(false);
    let stop = |message: String| {
        if message.starts_with("復原中") {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    assert!(journal::undo(&journal, &cancel, &stop).is_err());
    let events = journal.events().unwrap();
    let reconciled: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "intent_reconciled")
        .collect();
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0]["id"], json!(second.id));
    assert_eq!(reconciled[0]["active"], json!(true));

    assert_eq!(undo(&journal).unwrap(), 1);
    assert_eq!(snapshot(&root), before);
    assert_eq!(
        names(&journal)
            .iter()
            .filter(|e| *e == "intent_reconciled")
            .count(),
        1,
        "the second undo must replay the reconciliation instead of redoing it"
    );
}

// F023: every rejected event sequence stops before any undo entry or rename.
#[test]
fn invalid_event_sequences_stop_before_undo() {
    let _lock = exclusive();
    struct Ids {
        first: usize,
        second: usize,
        unchanged: usize,
        missing: usize,
    }
    type Case = (&'static str, fn(&Ids) -> Vec<Value>);
    let cases: [Case; 9] = [
        ("有多個未完成動作", |i| {
            vec![
                json!({"event":"rename_intent","id":i.first}),
                json!({"event":"rename_intent","id":i.second}),
            ]
        }),
        ("執行紀錄含重複改名動作", |i| {
            vec![
                json!({"event":"rename_intent","id":i.first}),
                json!({"event":"rename_done","id":i.first}),
                json!({"event":"rename_intent","id":i.first}),
            ]
        }),
        ("改名紀錄順序不合法", |i| {
            vec![json!({"event":"rename_done","id":i.first})]
        }),
        ("有未核對的中斷動作", |i| {
            vec![
                json!({"event":"rename_intent","id":i.first}),
                json!({"event":"undo_intent","id":i.first}),
            ]
        }),
        ("復原項目尚未改名", |i| {
            vec![json!({"event":"undo_intent","id":i.first})]
        }),
        ("復原紀錄順序不合法", |i| {
            vec![
                json!({"event":"rename_intent","id":i.first}),
                json!({"event":"rename_done","id":i.first}),
                json!({"event":"undo_done","id":i.first}),
            ]
        }),
        ("中斷核對紀錄不合法", |i| {
            vec![json!({"event":"intent_reconciled","id":i.first,"active":true})]
        }),
        ("執行紀錄包含不合法的項目", |i| {
            vec![json!({"event":"rename_intent","id":i.missing})]
        }),
        ("執行紀錄包含不合法的項目", |i| {
            vec![json!({"event":"rename_intent","id":i.unchanged})]
        }),
    ];
    for (expected, events) in cases {
        let base = fixture();
        let root = base.join("files");
        file(&root, "报告.txt");
        file(&root, "软件.txt");
        file(&root, "readme.txt");
        let before = snapshot(&root);
        let plan = plan(&root);
        let ids = Ids {
            first: row(&plan, "报告.txt").id,
            second: row(&plan, "软件.txt").id,
            unchanged: row(&plan, "readme.txt").id,
            missing: plan.rows.len(),
        };
        assert_eq!(plan.rows[ids.unchanged].status, Status::Unchanged);
        let journal = Journal::create(&base.join("history"), &plan).unwrap();
        journal.append("apply_start", json!({})).unwrap();
        for mut event in events(&ids) {
            let name = event["event"].as_str().unwrap().to_owned();
            event.as_object_mut().unwrap().remove("event");
            journal.append(&name, event).unwrap();
        }
        let error = message(undo(&journal).unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
        let prepared = journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {});
        assert!(message(prepared.unwrap_err()).contains(expected));
        assert_eq!(snapshot(&root), before, "{expected}");
        assert!(!names(&journal).iter().any(|e| e == "undo_start"));
    }
}

// F023: a damaged line followed by valid lines is reported with its position.
#[test]
fn damaged_middle_line_reports_file_and_line() {
    let _lock = exclusive();
    use std::io::Write;
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let renamed = snapshot(&root);
    let path = journal.directory.join("events.jsonl");
    let damaged_line = raw_events(&journal).lines().count() + 1;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"event\":\n{\"event\":\"later_valid_entry\"}\n")
        .unwrap();
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("執行紀錄損毀"), "{error}");
    assert!(error.contains(&format!("第 {damaged_line} 行")), "{error}");
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert!(!has_interrupted_tail(&journal));
    assert!(!raw_events(&journal).contains("undo_start"));
    assert_eq!(snapshot(&root), renamed);
}

// F066: an item moved back by another program after the undo preview leaves
// no unresolved undo entry, and the next undo continues.
#[test]
fn item_moved_back_during_undo_is_recorded_and_next_undo_continues() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    file(&root, "软件.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let (done, pending) = (row(&plan, "报告.txt"), row(&plan, "软件.txt"));
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":done.id}))
        .unwrap();
    let renamed = rename_row(done);
    journal
        .append(
            "rename_done",
            json!({"id":done.id,"identity":identity(&renamed)}),
        )
        .unwrap();
    journal
        .append("rename_intent", json!({"id":pending.id}))
        .unwrap();
    let moved = rename_row(pending);

    let original = PathBuf::from(&pending.path);
    let moved_back = Cell::new(false);
    let progress = |message: String| {
        if !moved_back.get()
            && (message.starts_with("核對復原順序") || message.starts_with("復原中"))
        {
            moved_back.set(true);
            native::rename_no_replace(&moved, &original, &identity(&moved)).unwrap();
        }
    };
    let error = message(journal::undo(&journal, &AtomicBool::new(false), &progress).unwrap_err());
    assert!(moved_back.get());
    assert!(error.contains("復原來源已變動"), "{error}");
    let state = journal::recover_state(&plan, &journal).unwrap();
    assert_eq!(state.pending, None, "no undo entry may be left unresolved");
    assert!(!state.active.contains(&pending.id));
    assert!(state.active.contains(&done.id));

    assert_eq!(undo(&journal).unwrap(), 1);
    assert_eq!(snapshot(&root), before);
}

// F066: a source replaced after the undo preview stops before any undo entry.
#[test]
fn replaced_source_during_undo_leaves_no_undo_entry() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let renamed = root.join("報告.txt");
    let replaced = Cell::new(false);
    let progress = |message: String| {
        if !replaced.get() && message.starts_with("復原中") {
            replaced.set(true);
            fs::remove_file(&renamed).unwrap();
            fs::write(&renamed, b"another document").unwrap();
        }
    };
    let error = message(journal::undo(&journal, &AtomicBool::new(false), &progress).unwrap_err());
    assert!(replaced.get());
    assert!(error.contains("復原來源已變動或被替換"), "{error}");
    assert!(!names(&journal).iter().any(|e| e == "undo_intent"));
    assert_eq!(fs::read(&renamed).unwrap(), b"another document");
    assert!(!Path::new(&row(&plan, "报告.txt").path).exists());
}

// F066: the event sequence left by a failed undo of a reconciled intent stays
// recoverable from the file system.
#[test]
fn failed_undo_after_reconciled_intent_remains_recoverable() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let id = row(&plan, "报告.txt").id;
    for (event, detail) in [
        ("apply_start", json!({})),
        ("rename_intent", json!({"id":id})),
        ("intent_reconciled", json!({"id":id,"active":true})),
        ("undo_start", json!({})),
        ("undo_intent", json!({"id":id})),
        ("undo_end", json!({"status":"stopped","count":0})),
    ] {
        journal.append(event, detail).unwrap();
    }
    let state = journal::recover_state(&plan, &journal).unwrap();
    assert_eq!(state.pending, Some(id));
    assert!(!state.active.contains(&id));
    assert_eq!(undo(&journal).unwrap(), 0);
    assert_eq!(snapshot(&root), before);
}

// F042: an unresolvable interruption names both paths and what was found.
#[test]
fn unresolvable_interruption_names_both_paths() {
    let _lock = exclusive();
    for hard_link in [false, true] {
        let base = fixture();
        let root = base.join("files");
        file(&root, "报告.txt");
        let plan = plan(&root);
        let journal = Journal::create(&base.join("history"), &plan).unwrap();
        let target = row(&plan, "报告.txt");
        journal.append("apply_start", json!({})).unwrap();
        journal
            .append("rename_intent", json!({"id":target.id}))
            .unwrap();
        let changed = rename_row(target);
        let original = PathBuf::from(&target.path);
        if hard_link {
            fs::hard_link(&changed, &original).unwrap();
        } else {
            fs::write(&changed, b"edited after the interrupted rename").unwrap();
        }
        let error =
            message(journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap_err());
        assert!(error.contains("無法唯一核對"), "{error}");
        assert!(error.contains(&original.display().to_string()), "{error}");
        assert!(error.contains(&changed.display().to_string()), "{error}");
        if hard_link {
            assert_eq!(error.matches("存在且身分相符").count(), 2, "{error}");
        } else {
            assert!(error.contains("不存在"), "{error}");
            assert!(error.contains("存在但身分不符"), "{error}");
        }
        assert!(!names(&journal).iter().any(|e| e == "intent_reconciled"));
    }
}

// F053: a failed end entry after every rename still reports the completed work.
#[test]
fn locked_journal_after_all_renames_reports_completion() {
    let _lock = exclusive();
    use std::os::windows::fs::OpenOptionsExt;
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/报告.txt");
    file(&root, "软件.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let total = ready(&plan).len();
    let events = journal.directory.join("events.jsonl");
    let renames = Cell::new(0);
    let lock = RefCell::new(None);
    let progress = |message: String| {
        if message.starts_with("改名中") {
            renames.set(renames.get() + 1);
        } else if message.starts_with("掃描中") && renames.get() == total && lock.borrow().is_none()
        {
            let exclusive = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&events)
                .unwrap();
            *lock.borrow_mut() = Some(exclusive);
        }
    };
    let result = journal::apply(&plan, &journal, &AtomicBool::new(false), &progress);
    assert!(lock.borrow_mut().take().is_some());
    let error = message(result.unwrap_err());
    assert!(
        error.starts_with(&format!("改名已全部完成（{total} 個）")),
        "{error}"
    );
    assert!(error.contains("結束紀錄寫入失敗"), "{error}");
    assert!(root.join("簡體目錄/報告.txt").exists());
    assert!(root.join("軟件.txt").exists());
    assert_eq!(undo(&journal).unwrap(), total);
    assert_eq!(snapshot(&root), before);
}

// F051: a scan-only record says so instead of reporting scope changes.
#[test]
fn scan_only_record_is_reported_as_not_applied() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    let before = snapshot(&root);
    let scanned = plan(&root);
    let scan_only = Journal::create(&base.join("history"), &scanned).unwrap();
    let error =
        message(journal::prepare_undo(&scan_only, &AtomicBool::new(false), &|_| {}).unwrap_err());
    assert!(error.contains("未執行改名"), "{error}");
    assert!(message(undo(&scan_only).unwrap_err()).contains("未執行改名"));
    assert!(!names(&scan_only).iter().any(|e| e == "undo_start"));
    assert_eq!(snapshot(&root), before);

    let applied_plan = plan(&root);
    let applied = Journal::create(&base.join("history"), &applied_plan).unwrap();
    journal::apply(&applied_plan, &applied, &AtomicBool::new(false), &|_| {}).unwrap();
    let error =
        message(journal::prepare_undo(&scan_only, &AtomicBool::new(false), &|_| {}).unwrap_err());
    assert!(error.contains("未執行改名"), "{error}");
    assert!(!error.contains("範圍已有新增"), "{error}");
}

// F010: incremental undo ordering matches a full rebuild for every step, and
// the ordering pass can be stopped without writing anything.
#[test]
fn nested_undo_order_matches_full_rebuild_and_can_be_stopped() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/软件资料/数据备份/文件夹/报告.txt");
    file(&root, "简体目录/软件资料/数据备份/图片.txt");
    file(&root, "简体目录/软件资料/计划.txt");
    file(&root, "简体目录/说明.txt");
    file(&root, "软件.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let count = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(count, ready(&plan).len());
    assert!(count >= 9);
    assert!(
        root.join("簡體目錄/軟件資料/數據備份/文件夾/報告.txt")
            .exists()
    );

    let order: Vec<usize> = journal
        .events()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "rename_intent")
        .map(|e| e["id"].as_u64().unwrap() as usize)
        .collect();
    let mut active: BTreeSet<usize> = order.iter().copied().collect();
    let mut expected = vec![];
    for &id in order.iter().rev() {
        let row = &plan.rows[id];
        let source = engine::mapped(&row.path, &engine::changes(&plan, &active));
        active.remove(&id);
        let target = engine::mapped(&row.path, &engine::changes(&plan, &active));
        expected.push((id, source, target));
    }
    let (_, actions) = journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let actual: Vec<_> = actions
        .iter()
        .map(|a| (a.id, a.source.clone(), a.target.clone()))
        .collect();
    assert_eq!(actual, expected);
    // Folders are restored before their contents, at four nested levels.
    let folders = ready(&plan).iter().filter(|r| r.kind == Kind::Dir).count();
    assert!(folders >= 4);
    for (index, (id, _, _)) in actual.iter().enumerate() {
        let path = Path::new(&plan.rows[*id].path);
        for (ancestor, _, _) in &actual[index + 1..] {
            assert!(!path.starts_with(&plan.rows[*ancestor].path));
        }
    }

    let recorded = raw_events(&journal);
    let cancel = AtomicBool::new(false);
    let stop = |message: String| {
        if message.starts_with("核對復原順序") {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    assert!(journal::prepare_undo(&journal, &cancel, &stop).is_err());
    cancel.store(false, Ordering::Relaxed);
    assert!(journal::undo(&journal, &cancel, &stop).is_err());
    assert_eq!(raw_events(&journal), recorded);

    assert_eq!(undo(&journal).unwrap(), count);
    assert_eq!(snapshot(&root), before);
}

// F014: an unrelated new item blocks undo with recovery advice, not rescan advice.
#[test]
fn unrelated_change_before_undo_points_to_preview_csv() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/报告.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let download = file(&root, "new-download.txt");
    let renamed = snapshot(&root);
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("preview.csv"), "{error}");
    assert!(!error.contains("請重新掃描"), "{error}");
    assert!(error.contains("new-download.txt"), "{error}");
    assert_eq!(snapshot(&root), renamed);
    assert!(!names(&journal).iter().any(|e| e == "undo_start"));

    fs::remove_file(download).unwrap();
    assert_eq!(undo(&journal).unwrap(), ready(&plan).len());
    assert_eq!(snapshot(&root), before);
}

// F014: a change during the last rename is reported as completed but unverified.
#[test]
fn change_during_last_rename_reports_completed_unverified() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/报告.txt");
    file(&root, "软件.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let total = ready(&plan).len();
    let renames = Cell::new(0);
    let export = root.join("export.csv");
    let progress = |message: String| {
        if message.starts_with("改名中") {
            renames.set(renames.get() + 1);
            if renames.get() == total {
                fs::write(&export, b"exported").unwrap();
            }
        }
    };
    let error =
        message(journal::apply(&plan, &journal, &AtomicBool::new(false), &progress).unwrap_err());
    assert!(error.starts_with("改名已全部完成"), "{error}");
    assert!(root.join("簡體目錄/報告.txt").exists());
    assert!(root.join("軟件.txt").exists());
    let events = journal.events().unwrap();
    let done: BTreeSet<_> = events
        .iter()
        .filter(|e| e["event"] == "rename_done")
        .map(|e| e["id"].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(done, ready(&plan).iter().map(|r| r.id).collect());
    let end = events.iter().rfind(|e| e["event"] == "apply_end").unwrap();
    assert_eq!(end["status"], json!("completed_unverified"));
    assert_eq!(end["count"], json!(total));

    fs::remove_file(export).unwrap();
    assert_eq!(undo(&journal).unwrap(), total);
    assert_eq!(snapshot(&root), before);
}

// Stopping during a scope check is reported as a stop, not as a scope change.
#[test]
fn stop_during_scope_check_is_not_reported_as_a_change() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let cancel = AtomicBool::new(false);
    let stop = |message: String| {
        if message.starts_with("掃描中") {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let error = message(journal::apply(&plan, &journal, &cancel, &stop).unwrap_err());
    assert!(error.contains("已停止"), "{error}");
    assert!(!error.contains("預覽後範圍已變動"), "{error}");
    assert!(!names(&journal).iter().any(|e| e == "apply_start"));

    cancel.store(false, Ordering::Relaxed);
    journal::apply(&plan, &journal, &cancel, &|_| {}).unwrap();
    let error = message(journal::prepare_undo(&journal, &cancel, &stop).unwrap_err());
    assert!(error.contains("已停止"), "{error}");
    assert!(!error.contains("無法自動復原"), "{error}");
}

// F038: empty folders, including convertible and nested ones, round-trip.
#[test]
fn empty_folders_are_renamed_and_restored() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    fs::create_dir_all(root.join("简体空目录")).unwrap();
    fs::create_dir_all(root.join("简体目录/软件资料/空文件夹")).unwrap();
    file(&root, "简体目录/报告.txt");
    let before = snapshot(&root);
    assert!(before.contains(&("简体空目录".to_owned(), None)));
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let count = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let renamed = snapshot(&root);
    assert!(renamed.contains(&("簡體空目錄".to_owned(), None)));
    assert!(renamed.contains(&("簡體目錄\\軟件資料\\空文件夾".to_owned(), None)));
    assert!(!renamed.iter().any(|(path, _)| path == "简体空目录"));
    assert_eq!(undo(&journal).unwrap(), count);
    assert_eq!(snapshot(&root), before);
}

// Identity overlay: identities recorded after renames are replayed.
#[test]
fn recorded_identities_are_replayed() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    let path = file(&root, "报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    let id = row(&plan, "报告.txt").id;
    let real = identity(&path);
    let (after_rename, after_undo) = (renumbered(&real), renumbered(&renumbered(&real)));
    assert_ne!(after_rename, after_undo);
    journal.append("apply_start", json!({})).unwrap();
    journal.append("rename_intent", json!({"id":id})).unwrap();
    journal
        .append("rename_done", json!({"id":id,"identity":after_rename}))
        .unwrap();
    let state = journal::recover_state(&plan, &journal).unwrap();
    assert!(state.active.contains(&id));
    assert_eq!(state.identities.get(&id), Some(&after_rename));
    journal.append("undo_intent", json!({"id":id})).unwrap();
    journal
        .append("undo_done", json!({"id":id,"identity":after_undo}))
        .unwrap();
    let state = journal::recover_state(&plan, &journal).unwrap();
    assert!(!state.active.contains(&id));
    assert_eq!(state.pending, None);
    assert_eq!(state.identities.get(&id), Some(&after_undo));
}

// Identity overlay: verification compares a row with its overlaid identity.
#[test]
fn verification_uses_the_identity_overlay() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "简体目录/报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let active: BTreeSet<usize> = ready(&plan).iter().map(|r| r.id).collect();
    let none = AtomicBool::new(false);
    let verify = |plan: &Plan, identities: &Identities| {
        engine::verify_with(plan, &active, identities, &none, &|_| {})
    };
    assert!(verify(&plan, &Identities::new()).is_ok());
    for name in ["报告.txt", "简体目录"] {
        let target = row(&plan, name);
        let current = engine::mapped(&target.path, &engine::changes(&plan, &active));
        let real = identity(&current);
        let scanned_on_fat = with_identity(plan.clone(), target.id, &renumbered(&real));
        assert!(
            verify(&scanned_on_fat, &Identities::new()).is_err(),
            "{name}"
        );
        let overlay = Identities::from([(target.id, real.clone())]);
        assert!(verify(&scanned_on_fat, &overlay).is_ok(), "{name}");
        let stale = Identities::from([(target.id, renumbered(&real))]);
        assert!(verify(&plan, &stale).is_err(), "{name}");
    }
}

// Identity overlay: a row whose file ID changed on rename is restored using
// the identity recorded in rename_done.
#[test]
fn undo_uses_identity_recorded_after_rename() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    let path = file(&root, "报告.txt");
    let before = snapshot(&root);
    let scanned = plan(&root);
    let id = row(&scanned, "报告.txt").id;
    let plan = with_identity(scanned, id, &renumbered(&identity(&path)));
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal.append("rename_intent", json!({"id":id})).unwrap();
    let source = PathBuf::from(&plan.rows[id].path);
    let target = source.with_file_name(&plan.rows[id].new);
    native::rename_no_replace(&source, &target, &identity(&source)).unwrap();
    journal
        .append("rename_done", json!({"id":id,"identity":identity(&target)}))
        .unwrap();
    assert_eq!(undo(&journal).unwrap(), 1);
    assert_eq!(snapshot(&root), before);
}

// The kind-and-content fallback is reserved for volumes that renumber items
// on rename (covered on real FAT32／exFAT by tests/fat_rust.rs). On NTFS a
// renumbered item after an interrupted rename is an unknown object and stops
// recovery without writing any event.
#[test]
fn renumbered_item_on_a_stable_volume_is_not_reconciled_after_rename() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    let path = file(&root, "报告.txt");
    let scanned = plan(&root);
    let id = row(&scanned, "报告.txt").id;
    let plan = with_identity(scanned, id, &renumbered(&identity(&path)));
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal.append("rename_intent", json!({"id":id})).unwrap();
    let source = PathBuf::from(&plan.rows[id].path);
    let target = source.with_file_name(&plan.rows[id].new);
    native::rename_no_replace(&source, &target, &identity(&source)).unwrap();
    let after_rename = snapshot(&root);
    let error = message(journal::recover_state(&plan, &journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert!(error.contains("存在但身分不符"), "{error}");
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert_eq!(snapshot(&root), after_rename);
    assert!(!names(&journal).iter().any(|e| e == "intent_reconciled"));
}

// Same gate for an interrupted undo on NTFS: the item carrying another file
// ID at the original name is not claimed.
#[test]
fn renumbered_item_on_a_stable_volume_is_not_reconciled_after_undo() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    let path = file(&root, "报告.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let id = row(&plan, "报告.txt").id;
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    for (event, detail) in [
        ("apply_start", json!({})),
        ("rename_intent", json!({"id":id})),
        (
            "rename_done",
            json!({"id":id,"identity":renumbered(&identity(&path))}),
        ),
        ("undo_start", json!({})),
        ("undo_intent", json!({"id":id})),
    ] {
        journal.append(event, detail).unwrap();
    }
    let error = message(journal::recover_state(&plan, &journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert_eq!(snapshot(&root), before);
    assert!(!names(&journal).iter().any(|e| e == "intent_reconciled"));
}

// The kind-and-content fallback never accepts a junction for a folder.
#[test]
fn junction_in_place_of_renamed_folder_is_not_reconciled() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    fs::create_dir_all(root.join("简体目录")).unwrap();
    let plan = plan(&root);
    let folder = row(&plan, "简体目录");
    assert_eq!(folder.kind, Kind::Dir);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":folder.id}))
        .unwrap();
    let elsewhere = base.join("moved-away");
    fs::rename(&folder.path, &elsewhere).unwrap();
    let junction = Path::new(&folder.path).with_file_name(&folder.new);
    let status = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&elsewhere)
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    assert!(native::metadata(&junction).unwrap().link);
    let error =
        message(journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert!(error.contains("存在但身分不符"), "{error}");
    fs::remove_dir(&junction).unwrap();
}

// journal-1: on NTFS the kind-and-content fallback is closed, so another
// empty folder found at the new name after an interrupted rename is not
// claimed as the renamed item.
#[test]
fn foreign_empty_folder_at_new_name_is_not_reconciled_on_ntfs() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    fs::create_dir_all(root.join("简体目录")).unwrap();
    let plan = plan(&root);
    let folder = row(&plan, "简体目录");
    assert_eq!(folder.kind, Kind::Dir);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":folder.id}))
        .unwrap();
    // The rename never happened; the folder was removed and an unrelated
    // empty folder was created under the new name.
    fs::remove_dir(&folder.path).unwrap();
    let foreign = Path::new(&folder.path).with_file_name(&folder.new);
    fs::create_dir(&foreign).unwrap();
    assert!(!native::metadata(&foreign).unwrap().link);
    let after = snapshot(&root);
    let error =
        message(journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert!(error.contains("存在但身分不符"), "{error}");
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert_eq!(snapshot(&root), after);
    assert!(!names(&journal).iter().any(|e| e == "intent_reconciled"));
}

// journal-1, reverse direction: after an interrupted undo, another empty
// folder at the original name is not claimed either.
#[test]
fn foreign_empty_folder_at_original_name_is_not_reconciled_on_ntfs() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    fs::create_dir_all(root.join("简体目录")).unwrap();
    let plan = plan(&root);
    let folder = row(&plan, "简体目录");
    assert_eq!(folder.kind, Kind::Dir);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append("rename_intent", json!({"id":folder.id}))
        .unwrap();
    let renamed = rename_row(folder);
    journal
        .append(
            "rename_done",
            json!({"id":folder.id,"identity":identity(&renamed)}),
        )
        .unwrap();
    journal.append("undo_start", json!({})).unwrap();
    journal
        .append("undo_intent", json!({"id":folder.id}))
        .unwrap();
    // The undo never happened; the renamed folder was removed and an
    // unrelated empty folder was created under the original name.
    fs::remove_dir(&renamed).unwrap();
    fs::create_dir(&folder.path).unwrap();
    let after = snapshot(&root);
    let error =
        message(journal::prepare_undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert!(error.contains("存在但身分不符"), "{error}");
    let error = message(undo(&journal).unwrap_err());
    assert!(error.contains("無法唯一核對"), "{error}");
    assert_eq!(snapshot(&root), after);
    assert!(!names(&journal).iter().any(|e| e == "intent_reconciled"));
}

/// Keeps a file in the classic delete-pending state, in which opening it (even
/// only to read attributes, with backup semantics) fails with access denied
/// instead of "not found". Dropping the guard clears the pending deletion
/// before closing the handle, so the file is never removed.
struct DeletePending(windows_sys::Win32::Foundation::HANDLE);
impl DeletePending {
    fn set(path: &Path) -> Self {
        use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::*};
        let name = native::wide(native::verbatim(path).as_os_str());
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                DELETE | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        let pending = Self(handle);
        assert!(pending.mark(true), "{}", std::io::Error::last_os_error());
        pending
    }
    /// FileDispositionInfo uses the classic (non-POSIX) semantics: the name
    /// stays until the last handle closes, and the flag can be cleared again.
    fn mark(&self, delete: bool) -> bool {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        };
        let info = FILE_DISPOSITION_INFO { DeleteFile: delete };
        unsafe {
            SetFileInformationByHandle(
                self.0,
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            ) != 0
        }
    }
}
impl Drop for DeletePending {
    fn drop(&mut self) {
        let cleared = self.mark(false);
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
        if !cleared && !std::thread::panicking() {
            panic!("無法取消測試檔案的刪除標記");
        }
    }
}

// journal-2: a source that exists but cannot be read is reported as
// unverifiable, not as changed or replaced, and leaves no undo entry.
#[test]
fn unreadable_undo_source_is_reported_as_unverifiable() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("files");
    file(&root, "报告.txt");
    let before = snapshot(&root);
    let plan = plan(&root);
    let journal = Journal::create(&base.join("history"), &plan).unwrap();
    journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    let renamed = root.join("報告.txt");
    let held: RefCell<Option<DeletePending>> = RefCell::new(None);
    let progress = |message: String| {
        if held.borrow().is_none() && message.starts_with("復原中") {
            let pending = DeletePending::set(&renamed);
            // Precondition: the read-attributes probe fails, and not with
            // "not found" (which undo reports as a moved item).
            let error = native::metadata(&renamed).unwrap_err();
            assert!(
                error.chain().any(|e| e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() != std::io::ErrorKind::NotFound)),
                "{error:#}"
            );
            *held.borrow_mut() = Some(pending);
        }
    };
    let result = journal::undo(&journal, &AtomicBool::new(false), &progress);
    let was_held = held.borrow_mut().take().is_some();
    assert!(was_held, "復原未進入逐項核對");
    let error = message(result.unwrap_err());
    assert!(error.contains("無法核對復原來源"), "{error}");
    assert!(!error.contains("已變動或被替換"), "{error}");
    assert!(!names(&journal).iter().any(|e| e == "undo_intent"));
    assert!(renamed.exists());
    // Once the source is readable again, the same record finishes the undo.
    assert_eq!(undo(&journal).unwrap(), 1);
    assert_eq!(snapshot(&root), before);
}
