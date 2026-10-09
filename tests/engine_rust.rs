//! Engine behaviour added by the review fixes: extensions, case collisions,
//! record-directory exclusion, unreadable names, path forms, trailing dots,
//! path length, cancellation and the dictionary lock.
use sc2tc_renamer::{
    converter::{Converter, Mode},
    engine::{self, Plan, Row, Status},
    journal::{self, Journal},
    native,
    updater::Store,
};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Once,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use uuid::Uuid;
use windows_sys::Win32::{Storage::FileSystem::*, System::Threading::CREATE_NO_WINDOW};

const TEST_NAME: &str = "engine_rust";
/// Other tests in this binary and other test processes on this machine share
/// the cross-process operation lock, which `apply` and `undo` take without
/// waiting.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// 600 attempts × 100 ms: wait up to one minute for those holders.
const LOCK_RETRY_LIMIT: u32 = 600;

/// Holds the operation lock for a whole test (same helper as
/// tests/journal_rust.rs). The mutex is re-entrant for its owning thread, so
/// `apply` and `undo` in the test still acquire it, while other threads and
/// processes wait instead of failing with the busy message.
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
/// Points the dictionary store at an empty fixture so no test reads or writes
/// the real %LOCALAPPDATA% dictionary state.
fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        Store::override_standard_root(work().join(format!("{TEST_NAME}-dictionary-store")))
            .unwrap();
    });
}
fn fixture() -> PathBuf {
    init();
    let path = work().join(TEST_NAME).join(Uuid::new_v4().to_string());
    fs::create_dir_all(&path).unwrap();
    path
}
fn path_of(root: &Path, relative: &str) -> PathBuf {
    root.join(relative.replace('/', "\\"))
}
fn write(root: &Path, relative: &str) -> PathBuf {
    let path = path_of(root, relative);
    fs::create_dir_all(native::verbatim(path.parent().unwrap())).unwrap();
    fs::write(native::verbatim(&path), b"synthetic document bytes\0\xff").unwrap();
    path
}
/// Every item below `root` (folders included), read through `\\?\` so names
/// with trailing dots are listed exactly; links are recorded, not followed.
fn snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut output = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(native::verbatim(&directory)).unwrap() {
            let entry = entry.unwrap();
            let path = directory.join(entry.file_name());
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                output.push((format!("{relative} -> link"), None));
            } else if kind.is_dir() {
                output.push((format!("{relative}\\"), None));
                pending.push(path);
            } else {
                output.push((relative, Some(fs::read(native::verbatim(&path)).unwrap())));
            }
        }
    }
    output.sort();
    output
}
fn plan(root: &Path) -> Plan {
    engine::make_plan(&[root.to_owned()], &AtomicBool::new(false), &|_| {}).unwrap()
}
fn row<'a>(plan: &'a Plan, path: &Path) -> &'a Row {
    plan.rows
        .iter()
        .find(|r| native::key(Path::new(&r.path)) == native::key(path))
        .unwrap_or_else(|| panic!("預覽缺少項目：{}", path.display()))
}
fn apply_and_undo(plan: &Plan, root: &Path, before: &[(String, Option<Vec<u8>>)]) -> usize {
    let journal = Journal::create(&fixture(), plan).unwrap();
    let count = journal::apply(plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        count
    );
    assert_eq!(snapshot(root), before);
    count
}
fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// A temporary SUBST drive letter, removed again when dropped.
struct Subst {
    drive: String,
}
impl Subst {
    fn map(target: &Path) -> Self {
        const LETTERS: &str = "ZYXWVUTSRQPONMLKJIHGF";
        for letter in LETTERS.chars() {
            let occupied = unsafe { GetLogicalDrives() };
            if occupied & (1 << (letter as u32 - 'A' as u32)) != 0 {
                continue;
            }
            let drive = format!("{letter}:");
            let created = Command::new("subst.exe")
                .arg(&drive)
                .arg(target)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .is_ok_and(|o| o.status.success());
            // Another process may take the same letter between the check and
            // SUBST. When SUBST fails the letter belongs to someone else, so
            // no guard is built: its Drop would remove their mapping.
            if !created {
                continue;
            }
            let mapped = Self { drive };
            // Only accept the letter when it resolves to the target; otherwise
            // dropping the guard removes the mapping this call created.
            if fs::canonicalize(mapped.root()).ok() == fs::canonicalize(target).ok() {
                return mapped;
            }
        }
        panic!("找不到可用的磁碟代號供 SUBST 測試使用");
    }
    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}\\", self.drive))
    }
}
impl Drop for Subst {
    fn drop(&mut self) {
        let _ = Command::new("subst.exe")
            .args([&self.drive, "/D"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
}

// F041: a dot followed by Chinese text is part of the name.
const EXTENSION_CASES: &[(&str, &str)] = &[
    ("第1.5版说明", "第1.5版說明"),
    ("说明.文档", "說明.文檔"),
    ("2024.03.15 会议记录", "2024.03.15 會議記錄"),
    ("软件说明.txt", "軟件說明.txt"),
    (".隐藏", ".隱藏"),
    ("数据备份.tar.gz", "數據備份.tar.gz"),
    ("报告.v2", "報告.v2"),
    ("报告.TXT", "報告.TXT"),
    ("报告.abcdefghijk", "報告.abcdefghijk"),
];

#[test]
fn preserved_extension_keeps_only_short_ascii_extensions() {
    for (name, expected) in [
        ("第1.5版说明", ""),
        ("说明.文档", ""),
        ("2024.03.15 会议记录", ""),
        ("软件说明.txt", ".txt"),
        (".隐藏", ""),
        ("数据备份.tar.gz", ".gz"),
        ("报告.v2", ".v2"),
        ("报告.TXT", ".TXT"),
        ("报告.my-ext_1", ".my-ext_1"),
        ("报告.abcdefghij", ".abcdefghij"),
        ("报告.abcdefghijk", ""),
        ("报告.", ""),
        ("报告", ""),
    ] {
        assert_eq!(engine::preserved_extension(name), expected, "{name}");
    }
}

#[test]
fn dotted_chinese_names_convert_whole_and_folders_match_files() {
    let base = fixture();
    for (name, _) in EXTENSION_CASES {
        write(&base, &format!("files/{name}"));
        fs::create_dir_all(path_of(&base, &format!("dirs/{name}"))).unwrap();
    }
    let plan = plan(&base);
    for (name, expected) in EXTENSION_CASES {
        for kind in ["files", "dirs"] {
            let item = row(&plan, &path_of(&base, &format!("{kind}/{name}")));
            assert_eq!(item.new, *expected, "{kind}: {name}");
            assert_eq!(item.status, Status::Ready, "{kind}: {name}");
        }
    }
}

#[test]
fn kelvin_sign_case_collision_is_reported_and_blocks_parent() {
    let _lock = exclusive();
    let base = fixture();
    // U+212A KELVIN SIGN lower-cases to "k" but NTFS upper-cases it to itself,
    // so both names coexist on an ordinary case-insensitive folder.
    let kelvin = write(&base, "简体目录/简\u{212A}.txt");
    let ascii = write(&base, "简体目录/简k.txt");
    write(&base, "简体目录/报告.txt");
    assert_eq!(native::key(&kelvin), native::key(&ascii));
    assert_ne!(
        native::metadata(&kelvin).unwrap().identity,
        native::metadata(&ascii).unwrap().identity
    );
    let before = snapshot(&base);
    let plan = plan(&base);
    assert!(
        plan.issues
            .iter()
            .any(|i| i.code == engine::ISSUE_CODE_CASE_COLLISION
                && native::key(Path::new(&i.path)) == native::key(&ascii)),
        "{:?}",
        plan.issues
    );
    for name in ["简\u{212A}.txt", "简k.txt"] {
        assert!(
            plan.rows
                .iter()
                .filter(|r| r.old == name)
                .all(|r| r.status != Status::Ready),
            "{name}"
        );
    }
    assert_eq!(row(&plan, &base.join("简体目录")).status, Status::Blocked);
    assert_eq!(
        row(&plan, &path_of(&base, "简体目录/报告.txt")).status,
        Status::Ready
    );
    assert_eq!(apply_and_undo(&plan, &base, &before), 1);
}

#[test]
fn case_sensitive_folder_collision_is_reported() {
    let _lock = exclusive();
    let base = fixture();
    let folder = base.join("简体目录");
    fs::create_dir(&folder).unwrap();
    let enabled = Command::new("fsutil.exe")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(&folder)
        .arg("enable")
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    if !enabled.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("略過：無法啟用區分大小寫資料夾（需要系統管理員或此系統不支援）：{enabled:?}");
        return;
    }
    let upper = write(&folder, "简A.txt");
    let lower = write(&folder, "简a.txt");
    assert_ne!(
        native::metadata(&upper).unwrap().identity,
        native::metadata(&lower).unwrap().identity
    );
    let before = snapshot(&base);
    let plan = plan(&base);
    assert!(
        plan.issues
            .iter()
            .any(|i| i.code == engine::ISSUE_CODE_CASE_COLLISION),
        "{:?}",
        plan.issues
    );
    assert!(
        plan.rows
            .iter()
            .filter(|r| r.old == "简A.txt" || r.old == "简a.txt")
            .all(|r| r.status != Status::Ready)
    );
    assert_eq!(row(&plan, &folder).status, Status::Blocked);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    assert_eq!(
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
    assert_eq!(snapshot(&base), before);
}

#[test]
fn inventory_excludes_tool_records_by_path_and_by_alias() {
    let base = fixture();
    let root = base.join("root");
    let history = root.join("fake-history");
    let record = write(&history, &format!("{}/plan.json", Uuid::new_v4()));
    write(&root, "简体.txt");
    let scopes = engine::normalise(std::slice::from_ref(&root)).unwrap();
    let scan = |exclusions: &[String]| {
        engine::inventory(
            &scopes,
            exclusions,
            &HashMap::new(),
            &AtomicBool::new(false),
            &|_| {},
            false,
        )
        .unwrap()
        .0
    };
    let check = |records: &[engine::Record], excluded: bool| {
        let history_record = records
            .iter()
            .find(|r| native::key(Path::new(&r.path)) == native::key(&history))
            .unwrap();
        let below = records.iter().any(|r| {
            native::contains(&history, Path::new(&r.path))
                && native::key(Path::new(&r.path)) != native::key(&history)
        });
        assert!(
            records
                .iter()
                .any(|r| r.path.ends_with("简体.txt") && r.protected.is_empty())
        );
        if excluded {
            assert_eq!(history_record.protected, "工具紀錄：保留");
            assert!(!below, "工具紀錄的子項目不應被掃描");
        } else {
            assert!(history_record.protected.is_empty());
            assert!(
                records
                    .iter()
                    .any(|r| native::key(Path::new(&r.path)) == native::key(&record))
            );
        }
    };
    // Control: an unrelated exclusion leaves the folder scannable.
    check(
        &scan(&[native::text(&base.join("elsewhere")).unwrap()]),
        false,
    );
    check(&scan(&[native::text(&history).unwrap()]), true);
    // Same directory through another spelling: only its identity matches.
    let alias = Subst::map(&history);
    assert!(!native::contains(&alias.root(), &history));
    check(&scan(&[native::text(&alias.root()).unwrap()]), true);
}

#[test]
fn scope_inside_history_root_is_refused_even_through_alias() {
    init();
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }
    let history = engine::history_root().unwrap();
    fs::create_dir_all(&history).unwrap();
    let inside = Remove(history.join(format!("{TEST_NAME}-{}", Uuid::new_v4().simple())));
    fs::create_dir(&inside.0).unwrap();
    let refused = |path: PathBuf| {
        let error = engine::make_plan(&[path], &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert!(
            format!("{error:#}").contains("工具的執行紀錄不能列入改名範圍"),
            "{error:#}"
        );
    };
    refused(inside.0.clone());
    let alias = Subst::map(&inside.0);
    refused(alias.root());
    drop(alias);
    assert_eq!(fs::read_dir(&inside.0).unwrap().count(), 0);
}

#[test]
fn unpaired_surrogate_name_is_an_issue_not_a_scan_failure() {
    let _lock = exclusive();
    const UNPAIRED_HIGH_SURROGATE: u16 = 0xD800;
    let base = fixture();
    let sibling = write(&base, "简体目录/报告.txt");
    let mut name = OsString::from_wide(&[UNPAIRED_HIGH_SURROGATE]);
    name.push(".txt");
    let invalid = native::verbatim(&base.join("简体目录")).join(&name);
    fs::write(&invalid, b"synthetic").unwrap();
    let before = snapshot(&base);
    let plan = plan(&base);
    let issue = plan
        .issues
        .iter()
        .find(|i| i.path.contains('\u{FFFD}'))
        .unwrap_or_else(|| panic!("{:?}", plan.issues));
    // The exact item was opened (not a U+FFFD look-alike that does not exist).
    assert!(
        issue.reason.contains("不是有效的 Unicode"),
        "{}",
        issue.reason
    );
    assert_eq!(row(&plan, &base.join("简体目录")).status, Status::Blocked);
    assert_eq!(row(&plan, &sibling).status, Status::Ready);
    // The issue is reproduced on every scan, so verification stays stable.
    assert_eq!(apply_and_undo(&plan, &base, &before), 1);
    fs::remove_file(&invalid).unwrap();
}

#[test]
fn path_helpers_handle_unc_verbatim_case_and_boundaries() {
    let p = Path::new;
    assert_eq!(native::key(p(r"C:\Data\Sub\")), r"c:\data\sub");
    assert_eq!(native::key(p("C:/Data/Sub")), r"c:\data\sub");
    assert_eq!(native::key(p(r"\\?\C:\Data")), r"c:\data");
    assert_eq!(
        native::key(p(r"\\?\UNC\Server\Share\A")),
        r"\\server\share\a"
    );
    assert_eq!(native::key(p(r"\\Server\Share\A\")), r"\\server\share\a");

    assert!(native::contains(p(r"C:\ab"), p(r"C:\AB\c")));
    assert!(native::contains(p(r"C:\ab\"), p(r"C:\ab")));
    assert!(!native::contains(p(r"C:\ab"), p(r"C:\abc")));
    assert!(!native::contains(p(r"C:\ab\c"), p(r"C:\ab")));
    assert!(native::contains(p(r"\\?\C:\ab"), p(r"C:\ab\x")));
    assert!(native::contains(p(r"\\s\sh"), p(r"\\?\UNC\s\sh\x")));
    assert!(!native::contains(p(r"\\s\sh"), p(r"\\s\share")));

    assert_eq!(native::verbatim(p(r"C:\a")), p(r"\\?\C:\a"));
    assert_eq!(native::verbatim(p("C:/a/b")), p(r"\\?\C:\a\b"));
    assert_eq!(native::verbatim(p(r"\\s\sh\a")), p(r"\\?\UNC\s\sh\a"));
    assert_eq!(native::verbatim(p(r"\\?\C:\a")), p(r"\\?\C:\a"));
    assert_eq!(native::verbatim(p(r"\\?\UNC\s\sh\a")), p(r"\\?\UNC\s\sh\a"));
    // Names that are not valid Unicode keep their exact UTF-16 units.
    let mut odd = OsString::from(r"C:\a\");
    odd.push(OsString::from_wide(&[0xD800]));
    let expected: Vec<u16> = r"\\?\".encode_utf16().chain(odd.encode_wide()).collect();
    assert_eq!(
        native::verbatim(Path::new(&odd))
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>(),
        expected
    );

    // `\\?\` (4 units) + `C:\a` (4) and `\\?\UNC\` (8) + `s\sh\a` (6).
    assert_eq!(native::path_units(p(r"C:\a")), 8);
    assert_eq!(native::path_units(p(r"\\s\sh\a")), 14);
    assert_eq!(native::path_units(p(r"\\?\C:\a")), 8);
    assert_eq!(native::path_units(p(r"\\?\UNC\s\sh\a")), 14);

    assert!(native::is_volume_root(p(r"D:\")));
    assert!(native::is_volume_root(p(r"\\?\D:\")));
    assert!(native::is_volume_root(p(r"\\server\share")));
    assert!(!native::is_volume_root(p(r"D:\a")));

    // F004: only a volume root folder may report file ID 0.
    assert_eq!(
        native::resolve_file_id(0, true, true).unwrap(),
        native::VOLUME_ROOT_FILE_ID
    );
    assert!(native::resolve_file_id(0, true, false).is_err());
    assert!(native::resolve_file_id(0, false, true).is_err());
    assert_eq!(native::resolve_file_id(5, false, false).unwrap(), 5);
    assert_eq!(native::resolve_file_id(5, true, true).unwrap(), 5);
}

#[test]
fn device_and_nt_object_paths_are_rejected() {
    let base = fixture();
    for path in [r"\\.\C:\x", r"\??\C:\x", r"\\?\GLOBALROOT\Device\x"] {
        let error = native::absolute(Path::new(path)).unwrap_err();
        assert!(
            error.to_string().contains("不支援裝置路徑"),
            "{path}: {error}"
        );
    }
    let device = PathBuf::from(format!(r"\\.\{}", base.display()));
    let error = engine::scope(&device).unwrap_err();
    assert!(error.to_string().contains("不支援裝置路徑"), "{error}");
    // Ordinary and `\\?\` spellings of the same folder remain accepted.
    let plain = engine::scope(&base).unwrap();
    let verbatim = engine::scope(&native::verbatim(&base)).unwrap();
    assert_eq!(
        native::key(Path::new(&plain.canonical)),
        native::key(Path::new(&verbatim.canonical))
    );
    assert_eq!(plain.anchor_id, verbatim.anchor_id);
}

#[test]
fn subst_root_protects_recycle_bin_and_program_folders() {
    let base = fixture();
    let recycle = base.join("$RECYCLE.BIN");
    fs::create_dir(&recycle).unwrap();
    assert_ne!(
        unsafe {
            SetFileAttributesW(
                native::wide(recycle.as_os_str()).as_ptr(),
                native::HIDDEN_SYSTEM,
            )
        },
        0
    );
    write(&base, ".git/config");
    fs::create_dir_all(base.join("简体目录").join("$recycle.bin")).unwrap();
    write(&base, "软件.txt");
    let drive = Subst::map(&base);
    let root = drive.root();
    let plan = plan(&root);
    // F004: an NTFS root keeps its real, non-zero file ID.
    let anchor = &plan.scopes[0].anchor_id;
    assert_ne!(anchor.file_id, 0);
    assert_ne!(anchor.file_id, native::VOLUME_ROOT_FILE_ID);
    let excluded = |relative: &str, reason: &str| {
        let item = row(&plan, &root.join(relative));
        assert_eq!(item.status, Status::Excluded, "{relative}");
        assert_eq!(item.reason, reason, "{relative}");
    };
    excluded("$RECYCLE.BIN", "隱藏或系統項目：保留");
    excluded(".git", "系統或程式資料夾：保留");
    excluded("简体目录\\$recycle.bin", "系統或程式資料夾：保留");
    assert_eq!(row(&plan, &root.join("简体目录")).status, Status::Blocked);
    assert_eq!(row(&plan, &root.join("软件.txt")).status, Status::Ready);
    assert!(
        !plan
            .records
            .iter()
            .any(|r| r.path.to_lowercase().contains("$recycle.bin\\")
                || r.path.ends_with("config"))
    );
}

#[test]
fn trailing_dot_folder_is_scanned_verbatim() {
    let _lock = exclusive();
    let base = fixture();
    // Alone: the folder named "资料." is listed as itself, not as "资料".
    let alone = base.join("alone");
    let dotted = write(&alone, "资料./报告.txt");
    let before = snapshot(&alone);
    let plan_alone = plan(&alone);
    assert!(plan_alone.issues.is_empty(), "{:?}", plan_alone.issues);
    assert!(
        plan_alone
            .records
            .iter()
            .any(|r| native::key(Path::new(&r.path)) == native::key(&dotted))
    );
    assert_eq!(row(&plan_alone, &dotted).status, Status::Ready);
    // "資料." would end with a dot, which Windows name rules refuse.
    let folder = row(&plan_alone, &alone.join("资料."));
    assert_eq!(folder.status, Status::Blocked);
    assert_eq!(folder.reason, "轉換後不符合 Windows 名稱規則");
    assert_eq!(apply_and_undo(&plan_alone, &alone, &before), 1);

    // Both spellings: each folder keeps its own children.
    let both = base.join("both");
    let dotted = write(&both, "资料./报告.txt");
    let plain = write(&both, "资料/软件.txt");
    let before = snapshot(&both);
    let plan_both = plan(&both);
    assert!(plan_both.issues.is_empty(), "{:?}", plan_both.issues);
    let keys: Vec<_> = plan_both
        .records
        .iter()
        .map(|r| native::key(Path::new(&r.path)))
        .collect();
    for present in [&dotted, &plain] {
        assert!(
            keys.contains(&native::key(present)),
            "{}",
            present.display()
        );
    }
    for absent in ["资料./软件.txt", "资料/报告.txt"] {
        assert!(
            !keys.contains(&native::key(&path_of(&both, absent))),
            "{absent}"
        );
    }
    assert_eq!(row(&plan_both, &both.join("资料")).status, Status::Ready);
    assert_eq!(row(&plan_both, &dotted).status, Status::Ready);
    assert_eq!(row(&plan_both, &plain).status, Status::Ready);
    let journal = Journal::create(&fixture(), &plan_both).unwrap();
    assert_eq!(
        journal::apply(&plan_both, &journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        3
    );
    assert!(native::metadata(&path_of(&both, "资料./報告.txt")).is_ok());
    assert!(native::metadata(&path_of(&both, "資料/軟件.txt")).is_ok());
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        3
    );
    assert_eq!(snapshot(&both), before);
    fs::remove_dir_all(native::verbatim(&base)).unwrap();
}

#[test]
fn growing_ancestor_past_the_path_limit_is_blocked() {
    // zh-TW converts 闪存盘 (3 units) to 快閃記憶體盤 (6 units).
    const GROWING_WORD: &str = "闪存盘";
    const GROWING_REPEAT: usize = 40;
    // The kernel expands `\??\C:` to `\Device\HarddiskVolumeN` (Microsoft
    // "Maximum Path Length Limitation"); keep the existing tree that far
    // below MAX_PATH_UNITS so it can still be created and scanned.
    const KERNEL_EXPANSION_MARGIN: usize = 64;
    const SEGMENT_UNITS: usize = 100;
    const MIN_LEAF_UNITS: usize = 130;
    const LEAF_AFFIXES: &str = "报告.txt";
    let base = fixture();
    let sibling = write(&base, "软件.txt");
    let name = GROWING_WORD.repeat(GROWING_REPEAT);
    let converted = Converter::with_mode(Mode::ZhTw)
        .unwrap()
        .convert(&name)
        .unwrap();
    let growth = units(&converted) - units(&name);
    assert!(units(&converted) <= native::MAX_COMPONENT_UNITS);
    assert!(growth > KERNEL_EXPANSION_MARGIN);
    let target = native::MAX_PATH_UNITS - KERNEL_EXPANSION_MARGIN;
    let growing = base.join(&name);
    let mut deepest = growing.clone();
    while native::path_units(&deepest) + 1 + SEGMENT_UNITS + 1 + MIN_LEAF_UNITS <= target {
        deepest.push("a".repeat(SEGMENT_UNITS));
    }
    // The leaf fills the remaining units exactly (130..=230, under 255).
    let leaf_units = target - native::path_units(&deepest) - 1;
    let leaf_name = format!("报告{}.txt", "b".repeat(leaf_units - units(LEAF_AFFIXES)));
    if let Err(error) = fs::create_dir_all(native::verbatim(&deepest)) {
        eprintln!("略過：無法建立接近上限的深路徑：{error}");
        let _ = fs::remove_dir_all(native::verbatim(&base));
        return;
    }
    let scan = || {
        engine::make_plan_with_mode(
            std::slice::from_ref(&base),
            Mode::ZhTw,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap()
    };
    // Control: without the leaf every final path still fits.
    assert!(native::path_units(&deepest) + growth < native::MAX_PATH_UNITS);
    assert_eq!(row(&scan(), &growing).status, Status::Ready);
    let leaf = deepest.join(&leaf_name);
    fs::write(native::verbatim(&leaf), b"synthetic").unwrap();
    assert_eq!(native::path_units(&leaf), target);
    assert!(native::path_units(&leaf) + growth >= native::MAX_PATH_UNITS);
    let plan = scan();
    for blocked in [&growing, &leaf] {
        let item = row(&plan, blocked);
        assert_eq!(item.status, Status::Blocked);
        assert_eq!(item.reason, "轉換後路徑超出 Windows 上限");
    }
    assert_eq!(row(&plan, &sibling).status, Status::Ready);
    fs::remove_dir_all(native::verbatim(&base)).unwrap();
}

#[test]
fn cancel_during_conflict_check_stops_plan() {
    let base = fixture();
    write(&base, "报告.txt");
    write(&base, "软件.txt");
    let cancel = AtomicBool::new(false);
    let checks = AtomicUsize::new(0);
    let progress = |message: String| {
        if message.starts_with("檢查同名") {
            checks.fetch_add(1, Ordering::Relaxed);
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let error = engine::make_plan(std::slice::from_ref(&base), &cancel, &progress).unwrap_err();
    assert!(error.to_string().contains("已停止"), "{error:#}");
    assert_eq!(checks.load(Ordering::Relaxed), 1);
}

#[test]
fn dictionary_lock_busy_reports_dictionary_update() {
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    const BUSY_WAIT_MS: u32 = 200;
    const FREE_WAIT_MS: u32 = 10_000;
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let lock = native::OperationLock::dictionary().unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(HANDSHAKE_TIMEOUT);
        drop(lock);
    });
    held_rx.recv_timeout(HANDSHAKE_TIMEOUT).unwrap();
    let busy = native::OperationLock::dictionary_with_timeout(BUSY_WAIT_MS);
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let message = busy.err().expect("鎖已被占用時不應取得").to_string();
    assert!(message.contains("更新轉換表"), "{message}");
    assert_ne!(message, native::OPERATION_BUSY);
    assert!(native::OperationLock::dictionary_with_timeout(FREE_WAIT_MS).is_ok());
}
