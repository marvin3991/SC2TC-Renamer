//! FAT32 and exFAT volumes on a temporary VHDX: the root reports file ID 0
//! and renaming to a longer name assigns a new file ID. Needs administrator
//! rights for diskpart; without them (or without virtual disk support) each
//! test prints why and returns without failing.
use sc2tc_renamer::{
    converter::Mode,
    engine::{self, Plan, Status},
    journal::{self, Journal},
    native,
    updater::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Once, atomic::AtomicBool},
};
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::GetLogicalDrives, System::Threading::CREATE_NO_WINDOW,
};

const TEST_NAME: &str = "fat_rust";
/// Small test disk; diskpart on Windows 11 formats it as both FAT32 and exFAT.
const VHDX_MEGABYTES: u32 = 64;
const VOLUME_LABEL: &str = "SC2TCFAT";
const SYSTEM_VOLUME_INFORMATION: &str = "System Volume Information";
/// zh-TW converts 因特网 (3 units) to 網際網路 (4 units). Three repeats plus
/// ".txt" grow a name from 13 to 16 units, past one FAT long-name entry (13
/// units) and one exFAT name entry (15 units), so the item needs more
/// directory entries and receives a new file ID.
const GROWING_WORD: &str = "因特网";
/// zh-TW converts 乒乓球 to 桌球 (one unit shorter).
const SHRINKING_NAME: &str = "乒乓球.txt";

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

enum DiskpartError {
    /// diskpart could not start (for example without administrator rights).
    Launch(String),
    Failed(String),
}
/// Runs one diskpart script. Each run takes tens of seconds while the
/// virtual disk service starts, so callers batch their commands.
fn diskpart(directory: &Path, lines: &[String]) -> Result<(), DiskpartError> {
    let script = directory.join(format!("diskpart-{}.txt", Uuid::new_v4().simple()));
    fs::write(&script, lines.join("\r\n")).map_err(|e| DiskpartError::Launch(e.to_string()))?;
    let output = Command::new("diskpart.exe")
        .arg("/s")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = fs::remove_file(&script);
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(DiskpartError::Failed(format!(
            "diskpart 結束代碼 {:?}：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout)
        ))),
        Err(error) => Err(DiskpartError::Launch(format!("無法執行 diskpart：{error}"))),
    }
}
fn free_letters() -> Vec<char> {
    const LETTERS: &str = "ZYXWVUTSRQPONMLKJIHGF";
    let occupied = unsafe { GetLogicalDrives() };
    LETTERS
        .chars()
        .filter(|&c| occupied & (1 << (c as u32 - 'A' as u32)) == 0)
        .collect()
}

/// A formatted VHDX attached with a drive letter; detached and deleted on drop.
struct VirtualVolume {
    directory: PathBuf,
    file: PathBuf,
    letter: Option<char>,
}
impl VirtualVolume {
    fn create(file_system: &str) -> Option<Self> {
        // Another process may take the chosen letter first; later attempts
        // only repeat the assignment on the already formatted volume.
        const ASSIGN_RETRIES: usize = 2;
        let directory = fixture();
        let file = directory.join("volume.vhdx");
        let mut volume = Self {
            directory,
            file,
            letter: None,
        };
        let vdisk = format!("select vdisk file=\"{}\"", volume.file.display());
        let letters = free_letters();
        let Some(&first) = letters.first() else {
            eprintln!("略過：沒有可用的磁碟代號");
            return None;
        };
        let created = diskpart(
            &volume.directory,
            &[
                format!(
                    "create vdisk file=\"{}\" maximum={VHDX_MEGABYTES} type=expandable",
                    volume.file.display()
                ),
                vdisk.clone(),
                "attach vdisk".to_owned(),
                "create partition primary".to_owned(),
                format!("format fs={file_system} quick label={VOLUME_LABEL}"),
                format!("assign letter={first}"),
            ],
        );
        match created {
            Ok(()) => {
                volume.letter = Some(first);
                return Some(volume);
            }
            Err(DiskpartError::Launch(error)) => {
                eprintln!("略過：無法執行 diskpart（需要系統管理員權限）：{error}");
                return None;
            }
            Err(DiskpartError::Failed(error)) if !volume.file.exists() => {
                eprintln!("略過：此環境無法建立虛擬磁碟：{error}");
                return None;
            }
            Err(DiskpartError::Failed(error)) => {
                eprintln!("第一次建立 {file_system} 磁碟未完成，改用其他代號：{error}");
            }
        }
        for letter in letters.into_iter().skip(1).take(ASSIGN_RETRIES) {
            let assigned = diskpart(
                &volume.directory,
                &[
                    vdisk.clone(),
                    "select partition 1".to_owned(),
                    format!("assign letter={letter}"),
                ],
            );
            if assigned.is_ok() {
                volume.letter = Some(letter);
                return Some(volume);
            }
        }
        eprintln!("略過：無法完成 {file_system} 虛擬磁碟的格式化或代號指派");
        None
    }
    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}:\\", self.letter.unwrap()))
    }
}
impl Drop for VirtualVolume {
    fn drop(&mut self) {
        if !self.file.exists() {
            let _ = fs::remove_dir_all(&self.directory);
            return;
        }
        // Dismount-DiskImage takes seconds; diskpart is the slower fallback.
        let quoted = self.file.to_string_lossy().replace('\'', "''");
        let dismounted = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Dismount-DiskImage -ImagePath '{quoted}' -ErrorAction Stop | Out-Null"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| o.status.success());
        if !dismounted {
            let detached = diskpart(
                &self.directory,
                &[
                    format!("select vdisk file=\"{}\"", self.file.display()),
                    "detach vdisk".to_owned(),
                ],
            );
            if let Err(DiskpartError::Launch(error) | DiskpartError::Failed(error)) = detached {
                eprintln!("無法卸載測試虛擬磁碟 {}：{error}", self.file.display());
            }
        }
        if let Err(error) = fs::remove_dir_all(&self.directory) {
            eprintln!("無法刪除測試虛擬磁碟 {}：{error}", self.file.display());
        }
    }
}

fn write(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative.replace('/', "\\"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("synthetic {relative}")).unwrap();
    path
}
/// Every synthetic item below the root with file contents; the system folder
/// that Windows maintains on each mounted volume is left out.
fn snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut output = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == SYSTEM_VOLUME_INFORMATION {
                continue;
            }
            let path = directory.join(entry.file_name());
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            if entry.file_type().unwrap().is_dir() {
                output.push((format!("{relative}\\"), None));
                pending.push(path);
            } else {
                output.push((relative, Some(fs::read(&path).unwrap())));
            }
        }
    }
    output.sort();
    output
}
fn plan_tw(root: &Path) -> Plan {
    engine::make_plan_with_mode(
        &[root.to_owned()],
        Mode::ZhTw,
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap()
}
fn events(journal: &Journal, name: &str) -> Vec<Value> {
    journal
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e["event"] == name)
        .collect()
}
fn identity_of(event: &Value) -> native::Identity {
    serde_json::from_value(event["identity"].clone())
        .unwrap_or_else(|e| panic!("事件缺少 identity：{event} ({e})"))
}

fn prepare_volume(root: &Path) {
    // Windows creates this folder shortly after mounting; creating it first
    // keeps it from appearing between the scan and the apply-time check.
    fs::create_dir_all(root.join(SYSTEM_VOLUME_INFORMATION)).unwrap();
    let growing_file = format!("{}.txt", GROWING_WORD.repeat(3));
    let growing_folder = GROWING_WORD.repeat(4);
    write(root, "软件说明.txt");
    write(root, &format!("{growing_folder}/软件.txt"));
    write(root, &format!("{growing_folder}/{growing_file}"));
    write(root, &format!("{growing_folder}/{SHRINKING_NAME}"));
    write(root, "子目录/报告.txt");
}

/// (a) the root is accepted with the sentinel identity; (b) a full apply and
/// undo succeed although FAT renumbers renamed items.
fn root_rename_and_recovery(volume: &VirtualVolume) {
    let root = volume.root();
    prepare_volume(&root);
    let before = snapshot(&root);
    let plan = plan_tw(&root);
    assert_eq!(
        plan.scopes[0].anchor_id.file_id,
        native::VOLUME_ROOT_FILE_ID
    );
    for record in &plan.records {
        if native::key(Path::new(&record.path)) != native::key(&root) {
            assert_ne!(record.identity.file_id, 0, "{}", record.path);
            assert_ne!(
                record.identity.file_id,
                native::VOLUME_ROOT_FILE_ID,
                "{}",
                record.path
            );
        }
    }
    let ready = plan
        .rows
        .iter()
        .filter(|r| r.status == Status::Ready)
        .count();
    const EXPECTED_READY: usize = 7;
    assert_eq!(ready, EXPECTED_READY, "{:?}", plan.rows);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let count = journal::apply(&plan, &journal, &AtomicBool::new(false), &|_| {})
        .unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(count, ready);
    assert!(
        root.join("網際網路網際網路網際網路網際網路/網際網路網際網路網際網路.txt")
            .exists()
    );
    assert!(
        root.join("網際網路網際網路網際網路網際網路/桌球.txt")
            .exists()
    );
    assert!(root.join("子目錄/報告.txt").exists());
    let done = events(&journal, "rename_done");
    assert_eq!(done.len(), count);
    let changed = done
        .iter()
        .filter(|e| {
            let id = e["id"].as_u64().unwrap() as usize;
            identity_of(e).file_id != plan.rows[id].identity.file_id
        })
        .count();
    assert!(changed > 0, "FAT 類檔案系統改成較長名稱後 file ID 應改變");
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {})
            .unwrap_or_else(|e| panic!("{e:#}")),
        count
    );
    assert_eq!(snapshot(&root), before);
    let restored = events(&journal, "undo_done");
    assert_eq!(restored.len(), count);
    for event in &restored {
        identity_of(event);
    }
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {}).unwrap(),
        0
    );
}

/// (c) an interrupted rename whose item was renumbered is recognised by its
/// unchanged kind, size and modification time.
fn interrupted_rename_is_reconciled(volume: &VirtualVolume) {
    let root = volume.root();
    let before = snapshot(&root);
    let plan = plan_tw(&root);
    let row = plan
        .rows
        .iter()
        .find(|r| r.status == Status::Ready && r.old == format!("{}.txt", GROWING_WORD.repeat(3)))
        .unwrap();
    let source = PathBuf::from(&row.path);
    let target = source.with_file_name(&row.new);
    let journal = Journal::create(&fixture(), &plan).unwrap();
    journal.append("apply_start", json!({})).unwrap();
    journal
        .append(
            "rename_intent",
            json!({"id":row.id,"source":native::text(&source).unwrap(),"target":native::text(&target).unwrap()}),
        )
        .unwrap();
    native::rename_no_replace(&source, &target, &row.identity).unwrap();
    assert_ne!(
        native::metadata(&target).unwrap().identity.file_id,
        row.identity.file_id,
        "前提：FAT 類檔案系統改成較長名稱後 file ID 應改變"
    );
    assert_eq!(
        journal::undo(&journal, &AtomicBool::new(false), &|_| {})
            .unwrap_or_else(|e| panic!("{e:#}")),
        1
    );
    assert_eq!(snapshot(&root), before);
    let reconciled = events(&journal, "intent_reconciled");
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0]["active"], json!(true));
    assert_ne!(identity_of(&reconciled[0]).file_id, row.identity.file_id);
}

#[test]
fn fat32_root_rename_interruption_and_recovery() {
    let Some(volume) = VirtualVolume::create("fat32") else {
        return;
    };
    root_rename_and_recovery(&volume);
    interrupted_rename_is_reconciled(&volume);
}

#[test]
fn exfat_root_rename_interruption_and_recovery() {
    let Some(volume) = VirtualVolume::create("exfat") else {
        return;
    };
    root_rename_and_recovery(&volume);
    interrupted_rename_is_reconciled(&volume);
}
