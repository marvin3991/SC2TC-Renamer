use crate::{
    engine::{self, Identities, Issue, Kind, Plan, Record, Row, Scope, Status},
    native::{self, Identity},
};
use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Journal {
    pub directory: PathBuf,
}
#[derive(Clone, Debug)]
pub struct UndoAction {
    pub id: usize,
    pub source: PathBuf,
    pub target: PathBuf,
}
/// Operation state replayed from events.jsonl.
#[derive(Clone, Debug, Default)]
pub struct RecoveredState {
    /// Rows currently carrying their new name.
    pub active: BTreeSet<usize>,
    /// Rows in the order their rename was attempted.
    pub order: Vec<usize>,
    /// A rename or undo whose completion was never recorded, reconciled
    /// against the file system.
    pub pending: Option<usize>,
    /// Latest identity recorded for each row after a rename or restore.
    pub identities: Identities,
}
struct UndoPlan {
    plan: Plan,
    actions: Vec<UndoAction>,
    state: RecoveredState,
}

const RESCAN_HINT: &str = "預覽後範圍已變動，請重新掃描產生新的預覽";
const MANUAL_RESTORE_HINT: &str = "範圍內有與本次改名無關的項目變動，無法自動復原；請移除新增的項目或還原變動後重試，或依 preview.csv 手動復原";

/// Adds `hint` to a failed scope check, unless the user stopped the check.
fn checked(result: Result<()>, cancel: &AtomicBool, hint: &'static str) -> Result<()> {
    match result {
        Err(error) if !cancel.load(Ordering::Relaxed) => Err(error.context(hint)),
        other => other,
    }
}

/// Explains a failed scope check before undo; rescanning cannot help a
/// recovery, so the hint points to manual restoration instead.
fn undo_checked(result: Result<()>, cancel: &AtomicBool) -> Result<()> {
    checked(result, cancel, MANUAL_RESTORE_HINT)
}

impl Journal {
    pub fn create(base: &Path, plan: &Plan) -> Result<Self> {
        let directory = base.join(format!(
            "{}-rust-{}",
            Local::now().format("%Y%m%d-%H%M%S"),
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(base)?;
        fs::create_dir(&directory)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("plan.json"))?;
        serde_json::to_writer_pretty(&mut file, plan)?;
        file.sync_all()?;
        engine::save_csv(plan, &directory.join("preview.csv"))?;
        let journal = Self { directory };
        journal.append("preview_saved", json!({}))?;
        Ok(journal)
    }
    pub fn load(&self) -> Result<Plan> {
        let value: Value = serde_json::from_slice(&fs::read(self.directory.join("plan.json"))?)?;
        let plan = if value["schema"] == 1 {
            legacy_plan(value)?
        } else {
            serde_json::from_value(value)?
        };
        engine::validate(&plan)?;
        Ok(plan)
    }
    pub fn events(&self) -> Result<Vec<Value>> {
        let path = self.directory.join("events.jsonl");
        if !path.exists() {
            return Ok(vec![]);
        }
        let raw = fs::read(&path)?;
        let mut result = vec![];
        let lines: Vec<&[u8]> = raw.split_inclusive(|byte| *byte == b'\n').collect();
        for (index, line) in lines.iter().enumerate() {
            match serde_json::from_slice(line) {
                Ok(value) => result.push(value),
                Err(_) if index + 1 == lines.len() && !line.ends_with(b"\n") => break,
                Err(_) => bail!(
                    "執行紀錄損毀，停止自動復原：{} 第 {} 行",
                    path.display(),
                    index + 1
                ),
            }
        }
        Ok(result)
    }
    pub fn append(&self, event: &str, mut detail: Value) -> Result<()> {
        let path = self.directory.join("events.jsonl");
        // Only inspect the last line; normal durable appends stay constant-time.
        if path.exists() {
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            let length = file.metadata()?.len();
            if length > 0 {
                use std::io::Read;
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    file.seek(SeekFrom::Start(0))?;
                    let mut raw = vec![];
                    file.read_to_end(&mut raw)?;
                    let split = raw.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
                    let tail = &raw[split..];
                    if serde_json::from_slice::<Value>(tail).is_err() {
                        let backup = self
                            .directory
                            .join(format!("interrupted-tail-{}.bin", Uuid::new_v4().simple()));
                        let mut copy = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(backup)?;
                        copy.write_all(tail)?;
                        copy.sync_all()?;
                        file.set_len(split as u64)?;
                        file.sync_all()?;
                    } else {
                        file.seek(SeekFrom::End(0))?;
                        file.write_all(b"\n")?;
                        file.sync_all()?;
                    }
                }
            }
        }
        detail["event"] = json!(event);
        detail["time"] = json!(Utc::now().to_rfc3339());
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        serde_json::to_writer(&mut file, &detail)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }
}

pub fn apply(
    plan: &Plan,
    journal: &Journal,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<usize> {
    engine::validate(plan)?;
    if engine::is_legacy_mode(&plan.mode) {
        bail!("這是舊版紀錄，只能復原；請重新掃描後改名。");
    }
    let _lock = native::OperationLock::acquire()?;
    if journal
        .events()?
        .iter()
        .any(|e| e["event"] == "apply_start")
    {
        bail!("這份預覽已執行過；不能重複套用，請重新掃描。");
    }
    if &journal.load()? != plan {
        bail!("畫面預覽與名稱備份不一致，停止。");
    }
    if !plan.legacy
        && !plan.dictionary_hash.is_empty()
        && crate::updater::Store::standard()?.current()?.1 != plan.dictionary_hash
    {
        bail!("字典版本已變更，請重新掃描，避免套用舊字典的預覽。");
    }
    checked(
        engine::verify(plan, &BTreeSet::new(), cancel, progress),
        cancel,
        RESCAN_HINT,
    )?;
    journal.append("apply_start", json!({}))?;
    let mut active = BTreeSet::new();
    let mut mapping = HashMap::new();
    let mut identities = Identities::new();
    let mut directories = engine::directory_index(plan);
    let mut rows: Vec<_> = plan
        .rows
        .iter()
        .filter(|r| r.status == Status::Ready)
        .collect();
    let total = rows.len();
    rows.sort_by_key(|r| {
        (
            std::cmp::Reverse(Path::new(&r.path).components().count()),
            r.id,
        )
    });
    let renamed = (|| -> Result<()> {
        for row in rows {
            engine::cancelled(cancel)?;
            let source = engine::mapped(&row.path, &mapping);
            let target = source.with_file_name(&row.new);
            progress(format!("改名中 · {}", source.display()));
            journal.append("rename_intent",json!({"id":row.id,"source":native::text(&source)?,"target":native::text(&target)?}))?;
            engine::move_row(
                plan,
                row,
                &row.identity,
                &source,
                &target,
                &mapping,
                &directories,
            )?;
            // FAT family volumes assign a new file ID on every rename; keep the
            // identity the item has now so verification and recovery can find it.
            let identity = native::metadata(&target)?.identity;
            active.insert(row.id);
            mapping.insert(native::key(Path::new(&row.path)), row.new.clone());
            if row.kind == Kind::Dir {
                directories.insert(native::key(Path::new(&row.path)), identity.clone());
            }
            identities.insert(row.id, identity.clone());
            journal.append("rename_done", json!({"id":row.id,"identity":identity}))?;
        }
        Ok(())
    })();
    if let Err(error) = renamed {
        let _ = journal.append(
            "apply_end",
            json!({"status":"stopped","count":active.len(),"reason":format!("{error:#}")}),
        );
        bail!(
            "已停止；完成 {} 個改名，共 {total} 個。保留此紀錄，可核對後復原。\n{error:#}",
            active.len()
        );
    }
    if let Err(error) = engine::verify_with(
        plan,
        &active,
        &identities,
        &AtomicBool::new(false),
        progress,
    ) {
        let _ = journal.append(
            "apply_end",
            json!({"status":"completed_unverified","count":active.len(),"reason":format!("{error:#}")}),
        );
        bail!(
            "改名已全部完成（{} 個），但完成後核對發現範圍有其他變動；紀錄已保留，可用於復原。\n{error:#}",
            active.len()
        );
    }
    journal
        .append(
            "apply_end",
            json!({"status":"complete","count":active.len()}),
        )
        .with_context(|| {
            format!(
                "改名已全部完成（{} 個），但結束紀錄寫入失敗；紀錄仍可用於復原",
                active.len()
            )
        })?;
    Ok(active.len())
}

fn operation_id(event: &Value, plan: &Plan) -> Result<usize> {
    let id = event["id"].as_u64().context("執行紀錄缺少項目索引")? as usize;
    if !plan.rows.get(id).is_some_and(|r| r.status == Status::Ready) {
        bail!("執行紀錄包含不合法的項目。");
    }
    Ok(id)
}

fn recorded_identity(event: &Value) -> Result<Option<Identity>> {
    if event["identity"].is_null() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_value(event["identity"].clone())
            .context("執行紀錄的項目身分格式不合法")?,
    ))
}

enum Probe {
    Missing,
    Matches,
    Other(native::Metadata),
}
fn probe(path: &Path, expected: &Identity) -> Result<Probe> {
    match native::metadata(path) {
        Ok(info) if !info.link && info.identity == *expected => Ok(Probe::Matches),
        Ok(info) => Ok(Probe::Other(info)),
        Err(error)
            if error.chain().any(|e| {
                e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(Probe::Missing)
        }
        Err(error) => Err(error),
    }
}
fn describe(probe: &Probe) -> &'static str {
    match probe {
        Probe::Missing => "不存在",
        Probe::Matches => "存在且身分相符",
        Probe::Other(_) => "存在但身分不符",
    }
}

/// Decides whether an interrupted operation moved the item, using the
/// recorded identity first and, on volumes that renumber items when they are
/// renamed, the unchanged kind, size and modification time. The fallback
/// applies to whichever name the interrupted rename or undo moved the item to,
/// and only while the other name is empty.
fn reconcile(
    row: &Row,
    expected: &Identity,
    original: &Path,
    changed: &Path,
) -> Result<(bool, Option<Identity>)> {
    let before = probe(original, expected)?;
    let after = probe(changed, expected)?;
    let renumbered = |info: &native::Metadata| {
        !info.link
            && info.directory == (row.kind == Kind::Dir)
            && info.identity.volume == expected.volume
            && info.identity.size == expected.size
            && info.identity.modified_ticks == expected.modified_ticks
    };
    let decision = match (&before, &after) {
        (Probe::Matches, Probe::Missing | Probe::Other(_)) => Some((false, None)),
        (Probe::Missing | Probe::Other(_), Probe::Matches) => Some((true, None)),
        (Probe::Missing, Probe::Other(info)) if renumbered(info) => {
            Some((true, Some(info.identity.clone())))
        }
        (Probe::Other(info), Probe::Missing) if renumbered(info) => {
            Some((false, Some(info.identity.clone())))
        }
        _ => None,
    };
    decision.with_context(|| {
        format!(
            "中斷動作無法唯一核對，停止自動復原。\n原名稱 {}：{}\n新名稱 {}：{}\n請手動確認哪一個名稱是正確的項目，移除或改回另一個後再按復原。",
            original.display(),
            describe(&before),
            changed.display(),
            describe(&after)
        )
    })
}

pub fn recover_state(plan: &Plan, journal: &Journal) -> Result<RecoveredState> {
    let mut state = RecoveredState::default();
    let mut seen = BTreeSet::new();
    let mut pending: Option<(bool, usize)> = None;
    for event in journal.events()? {
        match event["event"].as_str().context("執行紀錄格式不合法")? {
            "rename_intent" => {
                if pending.is_some() {
                    bail!("有多個未完成動作，停止自動復原。");
                }
                let id = operation_id(&event, plan)?;
                if !seen.insert(id) {
                    bail!("執行紀錄含重複改名動作。");
                }
                pending = Some((false, id));
                state.order.push(id);
            }
            "rename_done" => {
                let id = operation_id(&event, plan)?;
                if pending != Some((false, id)) {
                    bail!("改名紀錄順序不合法。");
                }
                state.active.insert(id);
                if let Some(identity) = recorded_identity(&event)? {
                    state.identities.insert(id, identity);
                }
                pending = None;
            }
            "undo_intent" => {
                if pending.is_some() {
                    bail!("有未核對的中斷動作，停止復原。");
                }
                let id = operation_id(&event, plan)?;
                if !state.active.contains(&id) {
                    bail!("復原項目尚未改名，停止。");
                }
                pending = Some((true, id));
            }
            "undo_done" => {
                let id = operation_id(&event, plan)?;
                if pending != Some((true, id)) {
                    bail!("復原紀錄順序不合法。");
                }
                state.active.remove(&id);
                if let Some(identity) = recorded_identity(&event)? {
                    state.identities.insert(id, identity);
                }
                pending = None;
            }
            "intent_reconciled" => {
                let id = operation_id(&event, plan)?;
                if pending.map(|p| p.1) != Some(id) {
                    bail!("中斷核對紀錄不合法。");
                }
                if event["active"].as_bool().context("中斷核對格式不合法")? {
                    state.active.insert(id);
                } else {
                    state.active.remove(&id);
                }
                if let Some(identity) = recorded_identity(&event)? {
                    state.identities.insert(id, identity);
                }
                pending = None;
            }
            _ => {}
        }
    }
    if let Some((_, id)) = pending {
        let row = &plan.rows[id];
        let mut off = state.active.clone();
        off.remove(&id);
        let mut on = state.active.clone();
        on.insert(id);
        let original = engine::mapped(&row.path, &engine::changes(plan, &off));
        let changed = engine::mapped(&row.path, &engine::changes(plan, &on));
        let expected = state
            .identities
            .get(&id)
            .cloned()
            .unwrap_or_else(|| row.identity.clone());
        let (renamed, identity) = reconcile(row, &expected, &original, &changed)?;
        state.active = if renamed { on } else { off };
        if let Some(identity) = identity {
            state.identities.insert(id, identity);
        }
        state.pending = Some(id);
    }
    Ok(state)
}

fn prepare_undo_state(
    journal: &Journal,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<UndoPlan> {
    let plan = journal.load()?;
    let state = recover_state(&plan, journal)?;
    if state.order.is_empty() {
        bail!("此紀錄只有掃描預覽，未執行改名，沒有可復原的項目；請改選有執行改名的那次紀錄。");
    }
    undo_checked(
        engine::verify_with(&plan, &state.active, &state.identities, cancel, progress),
        cancel,
    )?;
    let mut actions = vec![];
    let mut mapping = engine::changes(&plan, &state.active);
    for id in state.order.iter().rev().copied() {
        engine::cancelled(cancel)?;
        if !state.active.contains(&id) {
            continue;
        }
        let row = &plan.rows[id];
        progress(format!("核對復原順序 · {}", row.path));
        let source = engine::mapped(&row.path, &mapping);
        mapping.remove(&native::key(Path::new(&row.path)));
        let target = engine::mapped(&row.path, &mapping);
        actions.push(UndoAction { id, source, target });
    }
    Ok(UndoPlan {
        plan,
        actions,
        state,
    })
}

pub fn prepare_undo(
    journal: &Journal,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<(Plan, Vec<UndoAction>)> {
    let prepared = prepare_undo_state(journal, cancel, progress)?;
    Ok((prepared.plan, prepared.actions))
}

pub fn undo(journal: &Journal, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<usize> {
    let _lock = native::OperationLock::acquire()?;
    let UndoPlan {
        plan,
        actions,
        state,
    } = prepare_undo_state(journal, cancel, progress)?;
    let RecoveredState {
        mut active,
        pending,
        mut identities,
        ..
    } = state;
    if let Some(id) = pending {
        let mut detail = json!({"id":id,"active":active.contains(&id)});
        if let Some(identity) = identities.get(&id) {
            detail["identity"] = json!(identity);
        }
        journal.append("intent_reconciled", detail)?;
    }
    journal.append("undo_start", json!({}))?;
    let mut directories = engine::directory_index_with(&plan, &identities);
    let mut mapping = engine::changes(&plan, &active);
    let total = actions.len();
    let mut count = 0;
    let restored = (|| -> Result<()> {
        for action in actions {
            engine::cancelled(cancel)?;
            progress(format!("復原中 · {}", action.source.display()));
            let row = &plan.rows[action.id];
            let expected = identities
                .get(&action.id)
                .cloned()
                .unwrap_or_else(|| row.identity.clone());
            // Confirm the item before recording the intent, so a source that
            // changed since the preview never leaves an unfinished undo entry.
            let changed = || format!("復原來源已變動或被替換，停止：{}", action.source.display());
            match probe(&action.source, &expected).with_context(changed)? {
                Probe::Matches => {}
                Probe::Missing if matches!(probe(&action.target, &expected)?, Probe::Matches) => {
                    // Another program moved the item back after the preview.
                    // Record what the file system shows so the next undo
                    // continues from the actual state.
                    journal.append("undo_intent", json!({"id":action.id}))?;
                    journal.append(
                        "intent_reconciled",
                        json!({"id":action.id,"active":false,"identity":expected}),
                    )?;
                    bail!(
                        "{}\n項目已位於原名稱，已記錄；請確認範圍沒有其他程式在修改後再按復原。",
                        changed()
                    );
                }
                Probe::Missing | Probe::Other(_) => bail!(changed()),
            }
            journal.append("undo_intent", json!({"id":action.id}))?;
            engine::move_row(
                &plan,
                row,
                &expected,
                &action.source,
                &action.target,
                &mapping,
                &directories,
            )?;
            let identity = native::metadata(&action.target)?.identity;
            active.remove(&action.id);
            mapping.remove(&native::key(Path::new(&row.path)));
            if row.kind == Kind::Dir {
                directories.insert(native::key(Path::new(&row.path)), identity.clone());
            }
            identities.insert(action.id, identity.clone());
            count += 1;
            journal.append("undo_done", json!({"id":action.id,"identity":identity}))?;
        }
        Ok(())
    })();
    if let Err(error) = restored {
        let _ = journal.append(
            "undo_end",
            json!({"status":"stopped","count":count,"reason":format!("{error:#}")}),
        );
        bail!(
            "復原已停止；完成 {count} 個，共 {total} 個。保留此紀錄，可核對後繼續復原。\n{error:#}"
        );
    }
    if let Err(error) = engine::verify_with(
        &plan,
        &active,
        &identities,
        &AtomicBool::new(false),
        progress,
    ) {
        let _ = journal.append(
            "undo_end",
            json!({"status":"completed_unverified","count":count,"reason":format!("{error:#}")}),
        );
        bail!(
            "復原已全部完成（{count} 個），但完成後核對發現範圍有其他變動；紀錄已保留。\n{error:#}"
        );
    }
    journal
        .append("undo_end", json!({"status":"complete","count":count}))
        .with_context(|| format!("復原已全部完成（{count} 個），但結束紀錄寫入失敗"))?;
    Ok(count)
}

fn legacy_plan(value: Value) -> Result<Plan> {
    let mode = value["mode"].as_str().context("舊版模式缺失")?;
    if !engine::is_legacy_mode(mode) {
        bail!("舊版名稱備份的轉換模式不支援。");
    }
    let mode = if mode.ends_with(".json") {
        mode.to_owned()
    } else {
        format!("{mode}.json")
    };
    let roots: Vec<String> = serde_json::from_value(value["roots"].clone())?;
    let identity = |values: &Value, kind: Kind| -> Result<Identity> {
        let parts = values.as_array().context("舊版項目身分格式不合法")?;
        let volume = parts
            .first()
            .and_then(Value::as_u64)
            .context("舊版磁碟身分格式不合法")?;
        let file_id = parts
            .get(1)
            .context("舊版項目缺少身分")?
            .to_string()
            .parse::<u128>()
            .context("舊版項目身分格式不合法")?;
        let (size, modified_ticks) = if kind == Kind::File {
            let size = parts
                .get(2)
                .and_then(Value::as_u64)
                .context("舊版項目缺少檔案大小")?;
            let nanos = parts
                .get(3)
                .context("舊版項目缺少時間")?
                .to_string()
                .parse::<i128>()
                .context("舊版項目時間格式不合法")?;
            let ticks = nanos / native::NANOS_PER_FILETIME_TICK
                + i128::from(native::WINDOWS_UNIX_EPOCH_TICKS);
            (Some(size), Some(u64::try_from(ticks)?))
        } else {
            (None, None)
        };
        Ok(Identity {
            volume,
            file_id,
            size,
            modified_ticks,
        })
    };
    let full_path = |item: &Value| -> Result<String> {
        let root = item["root"].as_u64().context("舊版範圍索引不合法")? as usize;
        let mut path = PathBuf::from(roots.get(root).context("舊版範圍缺失")?);
        let parts: Vec<String> = serde_json::from_value(item["parts"].clone())?;
        for part in parts {
            if !native::valid_name(&part) {
                bail!("舊版相對路徑不合法。");
            }
            path.push(part);
        }
        native::text(&path)
    };
    let mut records = vec![];
    for item in value["records"]
        .as_array()
        .context("舊版名稱備份缺少項目")?
    {
        let kind: Kind = serde_json::from_value(item["kind"].clone())?;
        records.push(Record {
            path: full_path(item)?,
            kind,
            identity: identity(&item["identity"], kind)?,
            protected: item["protected"].as_str().unwrap_or("").to_owned(),
        });
    }
    let mut scopes = vec![];
    for root in &roots {
        let record = records
            .iter()
            .find(|r| native::key(Path::new(&r.path)) == native::key(Path::new(&root)))
            .context("舊版備份缺少根資料夾身分")?;
        scopes.push(Scope {
            path: root.clone(),
            kind: Kind::Dir,
            anchor: root.clone(),
            anchor_id: record.identity.clone(),
            canonical: root.clone(),
        });
    }
    let mut rows = vec![];
    for item in value["rows"].as_array().context("舊版備份缺少改名項目")? {
        let kind = serde_json::from_value(item["kind"].clone())?;
        rows.push(Row {
            id: item["id"].as_u64().context("舊版改名索引不合法")? as usize,
            path: full_path(item)?,
            kind,
            identity: identity(&item["identity"], kind)?,
            old: item["old"].as_str().context("舊版原名稱缺失")?.to_owned(),
            new: item["new"].as_str().context("舊版新名稱缺失")?.to_owned(),
            status: serde_json::from_value(item["status"].clone())?,
            reason: item["reason"].as_str().unwrap_or("").to_owned(),
        });
    }
    let mut issues = vec![];
    for item in value["issues"].as_array().context("舊版問題清單缺失")? {
        issues.push(Issue {
            path: full_path(item)?,
            code: item["code"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| item["code"].to_string()),
            reason: item["reason"].as_str().unwrap_or("").to_owned(),
        });
    }
    Ok(Plan {
        schema: engine::SCHEMA,
        version: engine::APP_VERSION.to_owned(),
        mode,
        created: value["created"].as_str().unwrap_or("").to_owned(),
        scopes,
        records,
        issues,
        rows,
        exclusions: vec![],
        legacy: true,
        dictionary_version: "舊版紀錄".to_owned(),
        dictionary_hash: String::new(),
    })
}
