use crate::{
    converter::{Converter, Mode},
    native::{self, Identity},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SCHEMA: u32 = 2;
const ERROR_SAMPLE_LIMIT: usize = 3;
/// Only short ASCII extensions are kept verbatim; a dot followed by Chinese
/// text is part of the name and is converted with it.
const MAX_EXTENSION_CHARS: usize = 10;
pub const ISSUE_CODE_CASE_COLLISION: &str = "case-collision";
const EXCLUDED_NAMES: &[&str] = &[
    ".git",
    ".venv",
    "node_modules",
    "$recycle.bin",
    "system volume information",
    "target",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Link,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Unchanged,
    Excluded,
    Blocked,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub path: String,
    pub kind: Kind,
    pub anchor: String,
    pub anchor_id: Identity,
    pub canonical: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub path: String,
    pub kind: Kind,
    pub identity: Identity,
    pub protected: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub path: String,
    pub code: String,
    pub reason: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: usize,
    pub path: String,
    pub kind: Kind,
    pub identity: Identity,
    pub old: String,
    pub new: String,
    pub status: Status,
    pub reason: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub schema: u32,
    pub version: String,
    pub mode: String,
    pub created: String,
    pub scopes: Vec<Scope>,
    pub records: Vec<Record>,
    pub issues: Vec<Issue>,
    pub rows: Vec<Row>,
    pub exclusions: Vec<String>,
    #[serde(default)]
    pub legacy: bool,
    #[serde(default)]
    pub dictionary_version: String,
    #[serde(default)]
    pub dictionary_hash: String,
}

/// Identities observed after renames, keyed by row id. FAT family file
/// systems assign a new file ID on every rename, so verification and recovery
/// use the latest recorded identity instead of the one captured at scan time.
pub type Identities = HashMap<usize, Identity>;

pub fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("已停止；沒有繼續處理其他項目。");
    }
    Ok(())
}
pub fn history_root() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("找不到本機應用程式資料目錄")?)
            .join("SC2TC-Renamer/history"),
    )
}

pub fn scope(path: &Path) -> Result<Scope> {
    let path = native::absolute(path)?;
    let info = native::metadata(&path)?;
    if info.link {
        bail!("不能加入符號連結或接合點：{}", path.display());
    }
    let kind = if info.directory {
        Kind::Dir
    } else {
        Kind::File
    };
    let anchor = if kind == Kind::Dir {
        path.clone()
    } else {
        path.parent().context("檔案沒有上層資料夾")?.to_owned()
    };
    let anchor_info = native::metadata(&anchor)?;
    if anchor_info.link {
        bail!(
            "檔案所在的資料夾是符號連結或接合點，不能加入：{}",
            anchor.display()
        );
    }
    let canonical = fs::canonicalize(&path).context("無法確認路徑範圍")?;
    Ok(Scope {
        path: native::text(&path)?,
        kind,
        anchor: native::text(&anchor)?,
        anchor_id: anchor_info.identity,
        canonical: native::text(&canonical)?,
    })
}

pub fn normalise(paths: &[PathBuf]) -> Result<Vec<Scope>> {
    if paths.is_empty() {
        bail!("請加入至少一個檔案、資料夾或磁碟根目錄。");
    }
    let mut scopes: Vec<Scope> = vec![];
    for path in paths {
        let candidate = scope(path)?;
        if scopes.iter().any(|s| {
            native::key(Path::new(&s.canonical)) == native::key(Path::new(&candidate.canonical))
        }) {
            continue;
        }
        if scopes.iter().any(|s| {
            (s.kind == Kind::Dir
                && native::contains(Path::new(&s.canonical), Path::new(&candidate.canonical)))
                || (candidate.kind == Kind::Dir
                    && native::contains(Path::new(&candidate.canonical), Path::new(&s.canonical)))
        }) {
            bail!("範圍不能互相包含；請保留最上層資料夾即可。");
        }
        scopes.push(candidate);
    }
    Ok(scopes)
}

fn issue(path: &Path, error: &anyhow::Error) -> Issue {
    let code = error
        .chain()
        .find_map(|e| {
            e.downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
        })
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    Issue {
        path: path.to_string_lossy().into_owned(),
        code,
        reason: format!("{error:#}"),
    }
}

/// Tool record locations resolved by path and by directory identity, so an
/// alias (SUBST drive, junction, 8.3 short name) cannot bypass the exclusion.
struct Exclusions {
    paths: Vec<String>,
    ids: Vec<(u64, u128)>,
}
impl Exclusions {
    fn resolve(paths: &[String]) -> Self {
        let ids = paths
            .iter()
            .filter_map(|p| native::metadata(Path::new(p)).ok())
            .filter(|m| m.directory && !m.link)
            .map(|m| (m.identity.volume, m.identity.file_id))
            .collect();
        Self {
            paths: paths.to_vec(),
            ids,
        }
    }
    fn matches(&self, path: &Path, info: &native::Metadata) -> bool {
        self.paths
            .iter()
            .any(|x| native::contains(Path::new(x), path))
            || self
                .ids
                .contains(&(info.identity.volume, info.identity.file_id))
    }
}

fn protection(
    path: &Path,
    info: &native::Metadata,
    exclusions: &Exclusions,
    legacy: bool,
) -> String {
    if info.link {
        "連結／接合點：不進入、不改名".to_owned()
    } else if info.attributes & native::HIDDEN_SYSTEM != 0 {
        "隱藏或系統項目：保留".to_owned()
    } else if exclusions.matches(path, info) {
        "工具紀錄：保留".to_owned()
    } else if path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            EXCLUDED_NAMES.contains(&name.to_lowercase().as_str())
                && !(legacy && name.eq_ignore_ascii_case("target"))
        })
    {
        "系統或程式資料夾：保留".to_owned()
    } else {
        String::new()
    }
}

pub fn changes(plan: &Plan, active: &BTreeSet<usize>) -> HashMap<String, String> {
    active
        .iter()
        .map(|&id| {
            let row = &plan.rows[id];
            (native::key(Path::new(&row.path)), row.new.clone())
        })
        .collect()
}

pub fn mapped(path: &str, changes: &HashMap<String, String>) -> PathBuf {
    let mut original = PathBuf::new();
    let mut actual = PathBuf::new();
    for part in Path::new(path).components() {
        original.push(part.as_os_str());
        actual.push(part.as_os_str());
        if let Some(name) = changes.get(&native::key(&original)) {
            actual.set_file_name(name);
        }
    }
    actual
}

pub fn inventory(
    scopes: &[Scope],
    exclusions: &[String],
    current: &HashMap<String, String>,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
    legacy: bool,
) -> Result<(Vec<Record>, Vec<Issue>)> {
    let exclusions = Exclusions::resolve(exclusions);
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    let mut issues = vec![];
    for scope in scopes {
        cancelled(cancel)?;
        let anchor = mapped(&scope.anchor, current);
        let anchor_info = native::metadata(&anchor)?;
        if anchor_info.link {
            bail!(
                "選取範圍的上層資料夾已變成符號連結或接合點，停止：{}",
                anchor.display()
            );
        }
        if anchor_info.identity != scope.anchor_id {
            bail!("選取範圍的上層資料夾已被替換，停止：{}", anchor.display());
        }
        records.insert(
            native::key(&anchor),
            Record {
                path: native::text(&anchor)?,
                kind: Kind::Dir,
                identity: anchor_info.identity,
                protected: String::new(),
            },
        );
        let mut pending = vec![(mapped(&scope.path, current), true)];
        while let Some((path, selected)) = pending.pop() {
            cancelled(cancel)?;
            progress(format!("掃描中 · {}", path.display()));
            let info = match native::metadata(&path) {
                Ok(info) => info,
                Err(error) => {
                    issues.push(issue(&path, &error));
                    continue;
                }
            };
            let text = match native::text(&path) {
                Ok(text) => text,
                Err(error) => {
                    issues.push(issue(&path, &error));
                    continue;
                }
            };
            let kind = if info.link {
                Kind::Link
            } else if info.directory {
                Kind::Dir
            } else {
                Kind::File
            };
            let mut protected = if selected && kind == Kind::Dir {
                String::new()
            } else {
                protection(&path, &info, &exclusions, legacy)
            };
            let record_key = native::key(&path);
            if let Some(previous) = records.get(&record_key)
                && previous.identity != info.identity
            {
                // Two items whose names differ only by case cannot both be tracked.
                issues.push(Issue {
                    path: text.clone(),
                    code: ISSUE_CODE_CASE_COLLISION.to_owned(),
                    reason: "同一資料夾內有僅大小寫不同的名稱並存，無法安全處理".to_owned(),
                });
                protected = "名稱僅大小寫不同的項目並存：保留".to_owned();
            }
            records.insert(
                record_key,
                Record {
                    path: text,
                    kind,
                    identity: info.identity,
                    protected: protected.clone(),
                },
            );
            if kind == Kind::Dir && protected.is_empty() {
                let entries = match fs::read_dir(native::verbatim(&path)) {
                    Ok(entries) => entries,
                    Err(error) => {
                        issues.push(issue(&path, &error.into()));
                        continue;
                    }
                };
                let mut children = vec![];
                for entry in entries {
                    match entry {
                        Ok(entry) => children.push(path.join(entry.file_name())),
                        Err(error) => issues.push(issue(&path, &error.into())),
                    }
                }
                children.sort_by_key(|p| native::key(p));
                pending.extend(children.into_iter().rev().map(|p| (p, false)));
            }
        }
    }
    issues.sort_by(|a, b| (&a.path, &a.code).cmp(&(&b.path, &b.code)));
    issues.dedup();
    Ok((records.into_values().collect(), issues))
}

/// Returns the extension kept verbatim (with its dot), or an empty string when
/// the text after the last dot is not a short ASCII extension.
pub fn preserved_extension(name: &str) -> &str {
    let Some(extension) = Path::new(name).extension().and_then(OsStr::to_str) else {
        return "";
    };
    let count = extension.chars().count();
    if (1..=MAX_EXTENSION_CHARS).contains(&count)
        && extension
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        &name[name.len() - extension.len() - 1..]
    } else {
        ""
    }
}

pub fn make_plan(
    paths: &[PathBuf],
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<Plan> {
    make_plan_with_mode(paths, Mode::ZhHant, cancel, progress)
}
pub fn make_plan_with_mode(
    paths: &[PathBuf],
    mode: Mode,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<Plan> {
    let scopes = normalise(paths)?;
    let history = history_root()?;
    // The record directory must exist before scanning so that it is excluded
    // by identity even when the scope reaches it through an alias.
    fs::create_dir_all(&history).context("無法建立工具紀錄目錄")?;
    let legacy_history =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("找不到本機應用程式資料目錄")?)
            .join("OpenCCRenamer/history");
    let exclusions = vec![native::text(&history)?, native::text(&legacy_history)?];
    let excluded = Exclusions::resolve(&exclusions);
    for scope in &scopes {
        let inside_by_path = excluded.paths.iter().any(|x| {
            native::contains(Path::new(x), Path::new(&scope.path))
                || native::contains(Path::new(x), Path::new(&scope.canonical))
        });
        // The resolved path matters when the scope is an alias (SUBST,
        // junction) of a folder inside the record directory: the alias itself
        // has no ancestors there, and its resolved spelling may differ from
        // the LOCALAPPDATA text (redirected or linked profile folders).
        let mut ancestors = Path::new(&scope.path)
            .ancestors()
            .chain(Path::new(&scope.canonical).ancestors());
        let inside_by_identity = ancestors.any(|ancestor| {
            native::metadata(ancestor).is_ok_and(|m| {
                excluded
                    .ids
                    .contains(&(m.identity.volume, m.identity.file_id))
            })
        });
        if inside_by_path || inside_by_identity {
            bail!("工具的執行紀錄不能列入改名範圍。");
        }
    }
    let (records, issues) = inventory(
        &scopes,
        &exclusions,
        &HashMap::new(),
        cancel,
        progress,
        false,
    )?;
    let converter = Converter::with_mode(mode)?;
    let mut blocked_parents = BTreeSet::new();
    for record in &records {
        if !record.protected.is_empty() {
            for p in Path::new(&record.path).ancestors().skip(1) {
                blocked_parents.insert(native::key(p));
            }
        }
    }
    for problem in &issues {
        for p in Path::new(&problem.path).ancestors() {
            blocked_parents.insert(native::key(p));
        }
    }
    let mut rows = vec![];
    for record in &records {
        cancelled(cancel)?;
        let path = Path::new(&record.path);
        let selected_root = scopes
            .iter()
            .any(|s| s.kind == Kind::Dir && native::key(Path::new(&s.path)) == native::key(path));
        let anchor_only = !scopes.iter().any(|s| {
            if s.kind == Kind::Dir {
                native::contains(Path::new(&s.path), path)
            } else {
                native::key(Path::new(&s.path)) == native::key(path)
            }
        });
        if selected_root || anchor_only {
            continue;
        }
        let old = path
            .file_name()
            .and_then(OsStr::to_str)
            .context("名稱不是有效的 Unicode")?
            .to_owned();
        let suffix = if record.kind == Kind::File {
            preserved_extension(&old)
        } else {
            ""
        };
        let new = converter.convert(&old[..old.len() - suffix.len()])? + suffix;
        let mut row = Row {
            id: rows.len(),
            path: record.path.clone(),
            kind: record.kind,
            identity: record.identity.clone(),
            old: old.clone(),
            new,
            status: Status::Unchanged,
            reason: "名稱不變".to_owned(),
        };
        if !record.protected.is_empty() {
            row.status = Status::Excluded;
            row.reason = record.protected.clone();
            row.new = old;
        } else if row.new != row.old {
            row.status = Status::Ready;
            row.reason.clear();
            if !native::valid_name(&row.new) {
                row.status = Status::Blocked;
                row.reason = "轉換後不符合 Windows 名稱規則".to_owned();
            } else if row.kind == Kind::Dir && blocked_parents.contains(&native::key(path)) {
                row.status = Status::Blocked;
                row.reason = "包含排除或無法讀取項目，上層名稱保留".to_owned();
            }
        }
        rows.push(row);
    }
    let mut targets: HashMap<String, usize> = HashMap::new();
    for row in &rows {
        if row.status == Status::Ready {
            *targets
                .entry(native::key(&Path::new(&row.path).with_file_name(&row.new)))
                .or_default() += 1;
        }
    }
    for row in &mut rows {
        if row.status != Status::Ready {
            continue;
        }
        cancelled(cancel)?;
        progress(format!("檢查同名 · {}", row.path));
        let target = Path::new(&row.path).with_file_name(&row.new);
        let occupied = match fs::symlink_metadata(native::verbatim(&target)) {
            Ok(_) => native::key(&target) != native::key(Path::new(&row.path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                row.status = Status::Blocked;
                row.reason = "無法確認目標名稱是否已被占用".to_owned();
                continue;
            }
        };
        if occupied || targets[&native::key(&target)] > 1 {
            row.status = Status::Conflict;
            row.reason = "轉換後同名或目標已存在，保留原項目".to_owned();
        }
    }
    let mut plan = Plan {
        schema: SCHEMA,
        version: APP_VERSION.to_owned(),
        mode: mode.config().to_owned(),
        created: Utc::now().to_rfc3339(),
        scopes,
        records,
        issues,
        rows,
        exclusions,
        legacy: false,
        dictionary_version: converter.dictionary_version.clone(),
        dictionary_hash: converter.dictionary_hash.clone(),
    };
    block_long_paths(&mut plan, cancel)?;
    validate(&plan)?;
    Ok(plan)
}

/// Every item keeps a legal path after all ready renames, including children
/// of renamed folders that are not renamed themselves.
fn block_long_paths(plan: &mut Plan, cancel: &AtomicBool) -> Result<()> {
    const REASON: &str = "轉換後路徑超出 Windows 上限";
    let active = plan
        .rows
        .iter()
        .filter(|r| r.status == Status::Ready)
        .map(|r| r.id)
        .collect();
    let mapping = changes(plan, &active);
    let row_by_key: HashMap<String, usize> = plan
        .rows
        .iter()
        .map(|r| (native::key(Path::new(&r.path)), r.id))
        .collect();
    let mut blocked = BTreeSet::new();
    for record in &plan.records {
        cancelled(cancel)?;
        if native::path_units(&mapped(&record.path, &mapping)) < native::MAX_PATH_UNITS {
            continue;
        }
        for ancestor in Path::new(&record.path).ancestors() {
            if let Some(&id) = row_by_key.get(&native::key(ancestor))
                && plan.rows[id].status == Status::Ready
            {
                blocked.insert(id);
            }
        }
    }
    for id in blocked {
        plan.rows[id].status = Status::Blocked;
        plan.rows[id].reason = REASON.to_owned();
    }
    Ok(())
}

pub fn is_legacy_mode(mode: &str) -> bool {
    matches!(mode, "s2tw" | "s2tw.json" | "s2twp" | "s2twp.json")
}

pub fn validate(plan: &Plan) -> Result<()> {
    if Mode::parse(&plan.mode).is_err() && !is_legacy_mode(&plan.mode) {
        bail!("名稱備份的轉換模式不支援。");
    }
    if plan.schema != SCHEMA || plan.scopes.is_empty() {
        bail!("名稱備份格式或選取範圍不符。");
    }
    let mut records = BTreeMap::new();
    for record in &plan.records {
        let path = Path::new(&record.path);
        if !path.is_absolute()
            || path.components().any(|p| {
                matches!(
                    p,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || record.identity.file_id == 0
        {
            bail!("名稱備份的路徑或項目身分不合法。");
        }
        if records.insert(native::key(path), record).is_some() {
            bail!("名稱備份包含重複路徑。");
        }
    }
    let mut seen = BTreeSet::new();
    for (index, row) in plan.rows.iter().enumerate() {
        let path = Path::new(&row.path);
        let original = records
            .get(&native::key(path))
            .context("名稱備份缺少原始項目")?;
        let in_scope = plan.scopes.iter().any(|s| {
            if s.kind == Kind::Dir {
                native::contains(Path::new(&s.path), path)
                    && native::key(Path::new(&s.path)) != native::key(path)
            } else {
                native::key(Path::new(&s.path)) == native::key(path)
            }
        });
        if row.id != index
            || !seen.insert(native::key(path))
            || !in_scope
            || path.file_name().and_then(OsStr::to_str) != Some(&row.old)
            || original.kind != row.kind
            || original.identity != row.identity
        {
            bail!("名稱備份的改名項目不合法。");
        }
        if row.status == Status::Ready
            && (!native::valid_name(&row.new)
                || row.kind == Kind::Link
                || !original.protected.is_empty())
        {
            bail!("名稱備份嘗試改名受保護項目，停止。");
        }
    }
    for scope in &plan.scopes {
        if !Path::new(&scope.path).is_absolute()
            || !Path::new(&scope.anchor).is_absolute()
            || scope.kind == Kind::Link
        {
            bail!("名稱備份的選取範圍不合法。");
        }
        if records
            .get(&native::key(Path::new(&scope.anchor)))
            .map(|r| &r.identity)
            != Some(&scope.anchor_id)
        {
            bail!("名稱備份缺少上層資料夾身分。");
        }
    }
    Ok(())
}

pub fn verify(
    plan: &Plan,
    active: &BTreeSet<usize>,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<()> {
    verify_with(plan, active, &Identities::new(), cancel, progress)
}

/// Compares the current tree with the plan; `identities` overrides the
/// scan-time identity of rows whose file ID changed when they were renamed.
pub fn verify_with(
    plan: &Plan,
    active: &BTreeSet<usize>,
    identities: &Identities,
    cancel: &AtomicBool,
    progress: &dyn Fn(String),
) -> Result<()> {
    let mapping = changes(plan, active);
    let (records, issues) = inventory(
        &plan.scopes,
        &plan.exclusions,
        &mapping,
        cancel,
        progress,
        plan.legacy,
    )?;
    let overlay: HashMap<String, &Identity> = identities
        .iter()
        .filter_map(|(&id, identity)| {
            plan.rows
                .get(id)
                .map(|row| (native::key(Path::new(&row.path)), identity))
        })
        .collect();
    let expected: BTreeMap<_, _> = plan
        .records
        .iter()
        .map(|r| {
            let identity = overlay
                .get(&native::key(Path::new(&r.path)))
                .copied()
                .unwrap_or(&r.identity);
            (
                native::key(&mapped(&r.path, &mapping)),
                (r.kind, identity, !r.protected.is_empty()),
            )
        })
        .collect();
    let actual: BTreeMap<_, _> = records
        .iter()
        .map(|r| {
            (
                native::key(Path::new(&r.path)),
                (r.kind, &r.identity, !r.protected.is_empty()),
            )
        })
        .collect();
    let issue_set = |items: &[Issue]| {
        items
            .iter()
            .map(|i| (native::key(Path::new(&i.path)), i.code.clone()))
            .collect::<BTreeSet<_>>()
    };
    if expected != actual {
        let sample = expected
            .keys()
            .chain(actual.keys())
            .filter(|p| expected.get(*p) != actual.get(*p))
            .take(ERROR_SAMPLE_LIMIT)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "範圍已有新增、移除、修改或替換的項目，停止。請重新掃描；復原時請保留紀錄查核。\n{sample}"
        );
    }
    let expected_issues = issue_set(&plan.issues);
    let current_issues = issue_set(&issues);
    if expected_issues != current_issues {
        let changed = expected_issues
            .symmetric_difference(&current_issues)
            .collect::<BTreeSet<_>>();
        let sample = issues
            .iter()
            .chain(plan.issues.iter())
            .filter(|issue| {
                changed.contains(&(native::key(Path::new(&issue.path)), issue.code.clone()))
            })
            .take(ERROR_SAMPLE_LIMIT)
            .map(|issue| format!("{} · {} · {}", issue.path, issue.code, issue.reason))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("掃描問題已變更，停止；請核對路徑與錯誤後重新掃描，復原時保留紀錄。\n{sample}");
    }
    Ok(())
}

pub fn directory_index(plan: &Plan) -> HashMap<String, Identity> {
    directory_index_with(plan, &Identities::new())
}

/// Directory identities keyed by original path, with renamed folders replaced
/// by the identity recorded after their rename.
pub fn directory_index_with(plan: &Plan, identities: &Identities) -> HashMap<String, Identity> {
    let mut index: HashMap<String, Identity> = plan
        .records
        .iter()
        .filter(|r| r.kind == Kind::Dir)
        .map(|r| (native::key(Path::new(&r.path)), r.identity.clone()))
        .collect();
    for (&id, identity) in identities {
        if let Some(row) = plan.rows.get(id)
            && row.kind == Kind::Dir
        {
            index.insert(native::key(Path::new(&row.path)), identity.clone());
        }
    }
    index
}

pub fn move_row(
    plan: &Plan,
    row: &Row,
    expected: &Identity,
    source: &Path,
    destination: &Path,
    mapping: &HashMap<String, String>,
    directories: &HashMap<String, Identity>,
) -> Result<()> {
    let scope = plan
        .scopes
        .iter()
        .find(|s| {
            if s.kind == Kind::Dir {
                native::contains(Path::new(&s.path), Path::new(&row.path))
            } else {
                native::key(Path::new(&s.path)) == native::key(Path::new(&row.path))
            }
        })
        .context("項目不在選取範圍")?;
    let mut original_parent = Path::new(&row.path)
        .parent()
        .context("來源缺少上層資料夾")?;
    loop {
        let parent = mapped(&native::text(original_parent)?, mapping);
        let expected_parent = directories
            .get(&native::key(original_parent))
            .context("名稱備份缺少上層資料夾身分")?;
        let info = native::metadata(&parent)?;
        if info.link || expected_parent != &info.identity {
            bail!("上層資料夾已被替換或變成連結，停止：{}", parent.display());
        }
        if native::key(original_parent) == native::key(Path::new(&scope.anchor)) {
            if info.identity != scope.anchor_id {
                bail!("選取範圍的身分已變動，停止。");
            }
            break;
        }
        original_parent = original_parent.parent().context("路徑超出選取範圍")?;
    }
    native::rename_no_replace(source, destination, expected)
}

pub fn save_csv(plan: &Plan, path: &Path) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&[0xef, 0xbb, 0xbf])?;
    let mut writer = csv::Writer::from_writer(file);
    writer.write_record(["狀態", "類型", "原路徑", "預計路徑", "原因"])?;
    let active = plan
        .rows
        .iter()
        .filter(|r| r.status == Status::Ready)
        .map(|r| r.id)
        .collect();
    let mapping = changes(plan, &active);
    for row in &plan.rows {
        writer.write_record([
            format!("{:?}", row.status),
            format!("{:?}", row.kind),
            row.path.clone(),
            native::text(&mapped(&row.path, &mapping))?,
            row.reason.clone(),
        ])?;
    }
    for issue in &plan.issues {
        writer.write_record(["Error", "", &issue.path, "", &issue.reason])?;
    }
    writer.flush()?;
    writer.into_inner()?.sync_all()?;
    Ok(())
}
