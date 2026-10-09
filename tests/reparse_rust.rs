//! Reparse points: junctions stay links that are never entered or renamed,
//! while reparse points that are not name surrogates (cloud placeholders,
//! WOF, third-party tags) are ordinary items.
use sc2tc_renamer::{
    engine::{self, Kind, Plan, Row, Status},
    journal::{self, Journal},
    native,
    updater::Store,
};
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::{Once, atomic::AtomicBool},
    time::Duration,
};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
    System::Threading::CREATE_NO_WINDOW,
};

const TEST_NAME: &str = "reparse_rust";
/// When set (CI sets it), a privileged test that cannot run fails instead of
/// being skipped, so a green run proves the reparse tests actually executed.
const REQUIRE_PRIVILEGED_TESTS: &str = "SC2TC_REQUIRE_PRIVILEGED_TESTS";
/// Other tests and test processes share the cross-process operation lock,
/// which `apply` and `undo` take without waiting.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// 600 attempts × 100 ms: wait up to one minute for those holders.
const LOCK_RETRY_LIMIT: u32 = 600;
/// Protection reason that v1.0.0 recorded for every reparse point
/// (`LINK_REASON` in src/engine.rs).
const V1_0_0_LINK_REASON: &str = "連結／接合點：不進入、不改名";
/// An ASCII name that the conversion leaves unchanged, so apply never renames
/// the reparse item itself.
const PLACEHOLDER_NAME: &str = "cloud-placeholder.bin";
/// winnt.h reparse tags.
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_CLOUD_6: u32 = 0x9000_601A;
const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
/// A non-Microsoft tag (bit 31 clear) without the name-surrogate bit; such
/// tags must use REPARSE_GUID_DATA_BUFFER (winnt.h).
const THIRD_PARTY_TAG: u32 = 0x0000_1234;
/// winioctl.h: CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 41, METHOD_BUFFERED, FILE_SPECIAL_ACCESS).
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
/// winnt.h REPARSE_GUID_DATA_BUFFER_HEADER_SIZE: tag, length, reserved, GUID.
const REPARSE_GUID_DATA_BUFFER_HEADER_SIZE: usize = 24;
/// Arbitrary GUID that identifies the synthetic third-party reparse data.
const SYNTHETIC_REPARSE_GUID: [u8; 16] = *b"SC2TC-test-guid!";

#[link(name = "kernel32")]
unsafe extern "system" {
    // Win32_System_IO is not an enabled windows-sys feature; declared from
    // ioapiset.h so the test does not change the product's dependencies.
    fn DeviceIoControl(
        device: HANDLE,
        control_code: u32,
        in_buffer: *const c_void,
        in_size: u32,
        out_buffer: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

/// Holds the operation lock for a whole test (same helper as
/// tests/journal_rust.rs). The mutex is re-entrant for its owning thread, so
/// `apply` and `undo` in the test still acquire it.
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
/// Reports a test that cannot run here: a failure when
/// `SC2TC_REQUIRE_PRIVILEGED_TESTS` is set, otherwise a printed skip. The
/// caller returns afterwards.
fn skip_or_fail(reason: &str) {
    if std::env::var_os(REQUIRE_PRIVILEGED_TESTS).is_some() {
        panic!("{REQUIRE_PRIVILEGED_TESTS} 已設定，特權測試不得略過：{reason}");
    }
    eprintln!("略過：{reason}");
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
fn write(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative.replace('/', "\\"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"synthetic document bytes\0\xff").unwrap();
    path
}
/// Relative names below `root`; links are listed but not followed, and file
/// contents are not opened (a third-party reparse file cannot be read).
fn names(root: &Path) -> Vec<String> {
    let mut output = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(native::verbatim(&directory)).unwrap() {
            let entry = entry.unwrap();
            let path = directory.join(entry.file_name());
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                output.push(format!("{relative} -> link"));
            } else if kind.is_dir() {
                output.push(format!("{relative}\\"));
                pending.push(path);
            } else {
                output.push(relative);
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
fn junction(link: &Path, target: &Path) {
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

#[test]
fn only_name_surrogate_tags_are_links() {
    for tag in [IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK] {
        assert!(native::is_name_surrogate(tag), "{tag:#x}");
    }
    for tag in [
        IO_REPARSE_TAG_CLOUD_6,
        IO_REPARSE_TAG_WOF,
        THIRD_PARTY_TAG,
        0,
    ] {
        assert!(!native::is_name_surrogate(tag), "{tag:#x}");
    }
}

#[test]
fn junction_is_rejected_as_scope_and_never_entered() {
    let _lock = exclusive();
    let base = fixture();
    let outside = base.join("outside").join("外部资料");
    write(&outside, "报告.txt");
    write(&outside, "子目录/说明.txt");
    let outside_before = names(&base.join("outside"));
    let root = base.join("root");
    write(&root, "简体目录/报告.txt");
    write(&root, "软件.txt");
    let link = root.join("简体目录").join("链接");
    junction(&link, &outside);

    let info = native::metadata(&link).unwrap();
    assert!(info.link);
    assert!(info.directory);
    assert_eq!(info.reparse_tag, IO_REPARSE_TAG_MOUNT_POINT);
    for selected in [link.clone(), link.join("报告.txt")] {
        let error = engine::scope(&selected).unwrap_err();
        assert!(
            error.to_string().contains("符號連結或接合點"),
            "{}: {error}",
            selected.display()
        );
    }
    // A junction above the file's own folder is not the scope's anchor.
    let nested = engine::scope(&link.join("子目录").join("说明.txt")).unwrap();
    assert_eq!(
        native::key(Path::new(&nested.anchor)),
        native::key(&link.join("子目录"))
    );

    let before = names(&root);
    let plan = plan(&root);
    assert!(!plan.records.iter().any(|r| {
        let path = Path::new(&r.path);
        native::contains(&base.join("outside"), path)
            || (native::contains(&link, path) && native::key(path) != native::key(&link))
    }));
    let linked = row(&plan, &link);
    assert_eq!(linked.kind, Kind::Link);
    assert_eq!(linked.status, Status::Excluded);
    assert!(linked.reason.contains("接合點"), "{}", linked.reason);
    assert_eq!(row(&plan, &root.join("简体目录")).status, Status::Blocked);
    assert_eq!(
        row(&plan, &root.join("简体目录").join("报告.txt")).status,
        Status::Ready
    );
    let journal = Journal::create(&fixture(), &plan).unwrap();
    assert_eq!(
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        2
    );
    assert!(root.join("简体目录").join("報告.txt").exists());
    assert_eq!(names(&base.join("outside")), outside_before);
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        2
    );
    assert_eq!(names(&root), before);
    assert_eq!(names(&base.join("outside")), outside_before);
}

#[test]
fn folder_replaced_by_junction_after_scan_stops_before_apply() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("root");
    write(&root, "简体目录/报告.txt");
    let plan = plan(&root);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let moved = base.join("moved");
    fs::rename(root.join("简体目录"), &moved).unwrap();
    junction(&root.join("简体目录"), &moved);
    let error = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap_err();
    assert!(
        format!("{error:#}").contains("預覽後範圍已變動"),
        "{error:#}"
    );
    assert!(
        journal
            .events()
            .unwrap()
            .iter()
            .all(|e| e["event"] != "apply_start")
    );
    assert!(moved.join("报告.txt").exists());
    assert!(!moved.join("報告.txt").exists());
}

/// Sets a third-party reparse point on an existing file; false when the
/// file system or account refuses it.
fn set_third_party_reparse_point(path: &Path) -> bool {
    let name = native::wide(native::verbatim(path).as_os_str());
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_WRITE_ATTRIBUTES | FILE_WRITE_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        eprintln!("無法開啟測試檔案：{}", std::io::Error::last_os_error());
        return false;
    }
    let mut buffer = [0_u8; REPARSE_GUID_DATA_BUFFER_HEADER_SIZE];
    buffer[..4].copy_from_slice(&THIRD_PARTY_TAG.to_le_bytes());
    // ReparseDataLength and Reserved stay 0: no private data follows the GUID.
    buffer[8..].copy_from_slice(&SYNTHETIC_REPARSE_GUID);
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    } != 0;
    if !ok {
        eprintln!(
            "FSCTL_SET_REPARSE_POINT 失敗：{}",
            std::io::Error::last_os_error()
        );
    }
    unsafe {
        CloseHandle(handle);
    }
    ok
}

#[test]
fn third_party_reparse_file_is_renamed_like_an_ordinary_file() {
    let _lock = exclusive();
    let base = fixture();
    let root = base.join("root");
    let source = write(&root, "报告.txt");
    let size = fs::metadata(&source).unwrap().len();
    if !set_third_party_reparse_point(&source) {
        skip_or_fail("此環境無法設定非名稱代理的 reparse point（需要相應權限）。");
        return;
    }
    let info = native::metadata(&source).unwrap();
    assert!(!info.link);
    assert!(!info.directory);
    assert_ne!(info.attributes & native::REPARSE_POINT, 0);
    assert_eq!(info.reparse_tag, THIRD_PARTY_TAG);
    assert_eq!(info.identity.size, Some(size));
    let before = names(&root);
    let plan = plan(&root);
    let item = row(&plan, &source);
    assert_eq!(item.kind, Kind::File);
    assert_eq!(item.status, Status::Ready);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    assert_eq!(
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        1
    );
    let renamed = native::metadata(&root.join("報告.txt")).unwrap();
    assert_eq!(renamed.reparse_tag, THIRD_PARTY_TAG);
    assert_eq!(renamed.identity, info.identity);
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        1
    );
    assert_eq!(names(&root), before);
    // The file cannot be opened without its (absent) filter driver; remove it
    // so later tools that walk work/ do not trip over it.
    fs::remove_dir_all(&base).unwrap();
}

/// A journal whose plan.json has the shape v1.0.0 wrote for a scope holding a
/// non-name-surrogate reparse file (cloud placeholder): one ordinary file was
/// renamed, and the reparse file was recorded as a protected link.
struct LegacyRecord {
    base: PathBuf,
    root: PathBuf,
    placeholder: PathBuf,
    journal: Journal,
    before: Vec<String>,
}

/// Applies the rename with the current version, then rewrites the record of
/// the reparse file as v1.0.0 did: kind "link", the link protection reason, and
/// an identity without size or modification time. The item is a file, so no
/// child records exist below it. `None` when the reparse point cannot be set.
fn legacy_reparse_record() -> Option<LegacyRecord> {
    let base = fixture();
    let root = base.join("root");
    write(&root, "报告.txt");
    let placeholder = write(&root, PLACEHOLDER_NAME);
    if !set_third_party_reparse_point(&placeholder) {
        skip_or_fail("此環境無法設定非名稱代理的 reparse point（需要相應權限）。");
        return None;
    }
    let before = names(&root);
    let plan = plan(&root);
    let item = row(&plan, &placeholder);
    assert_eq!(item.kind, Kind::File);
    assert_eq!(item.status, Status::Unchanged);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    assert_eq!(
        journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {})
            .unwrap_or_else(|e| panic!("{e:#}")),
        1
    );

    let file = journal.directory.join("plan.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let key = native::key(&placeholder);
    let is_placeholder =
        |item: &Value| native::key(Path::new(item["path"].as_str().unwrap())) == key;
    let as_v1_0_0 = |item: &mut Value| {
        item["kind"] = json!("link");
        let identity = item["identity"].as_object_mut().unwrap();
        identity.remove("size");
        identity.remove("modified_ticks");
    };
    let mut records = 0;
    for record in value["records"].as_array_mut().unwrap() {
        if is_placeholder(record) {
            as_v1_0_0(record);
            record["protected"] = json!(V1_0_0_LINK_REASON);
            records += 1;
        }
    }
    let mut rows = 0;
    for row in value["rows"].as_array_mut().unwrap() {
        if is_placeholder(row) {
            as_v1_0_0(row);
            row["status"] = json!("excluded");
            row["reason"] = json!(V1_0_0_LINK_REASON);
            rows += 1;
        }
    }
    assert_eq!((records, rows), (1, 1));
    fs::write(&file, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let rewritten = journal.load().unwrap();
    assert_eq!(row(&rewritten, &placeholder).kind, Kind::Link);
    Some(LegacyRecord {
        base,
        root,
        placeholder,
        journal,
        before,
    })
}

// native-engine-1: a v1.0.0 record that listed a cloud placeholder as a link
// still undoes after the item is classified as an ordinary file.
#[test]
fn v1_0_0_record_with_non_surrogate_reparse_file_can_be_undone() {
    let _lock = exclusive();
    let Some(legacy) = legacy_reparse_record() else {
        return;
    };
    let cancel = AtomicBool::new(false);
    let (_, actions) = journal::prepare_undo(&legacy.journal, &cancel, &|_| {})
        .unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(actions.len(), 1);
    assert_eq!(
        journal::undo(&legacy.journal, &cancel, &|_| {}).unwrap_or_else(|e| panic!("{e:#}")),
        1
    );
    assert_eq!(names(&legacy.root), legacy.before);
    fs::remove_dir_all(&legacy.base).unwrap();
}

// Control for native-engine-1: the compatibility rule applies only to items
// that are still non-surrogate reparse points. A junction put in place of the
// recorded item is a different object and stops the undo.
#[test]
fn v1_0_0_link_record_does_not_accept_a_junction_in_its_place() {
    let _lock = exclusive();
    let Some(legacy) = legacy_reparse_record() else {
        return;
    };
    fs::remove_file(&legacy.placeholder).unwrap();
    let elsewhere = legacy.base.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    junction(&legacy.placeholder, &elsewhere);
    assert!(native::metadata(&legacy.placeholder).unwrap().link);
    let error = journal::prepare_undo(&legacy.journal, &AtomicBool::new(false), &|_| {})
        .err()
        .expect("接合點取代紀錄中的項目時不得放行復原");
    let message = format!("{error:#}");
    assert!(
        message.contains("範圍已有新增、移除、修改或替換的項目"),
        "{message}"
    );
    assert!(legacy.root.join("報告.txt").exists());
    assert!(
        !legacy
            .journal
            .events()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "undo_start")
    );
    fs::remove_dir(&legacy.placeholder).unwrap();
    fs::remove_dir_all(&legacy.base).unwrap();
}
