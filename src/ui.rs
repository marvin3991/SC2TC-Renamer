use crate::{
    converter::Mode,
    engine::{self, Kind, Plan, Scope, Status},
    journal::{self, Journal, UndoAction},
    native,
    updater::{self, Bundle, Release, Store},
};
use anyhow::Result;
use eframe::egui::{
    self, Align, Color32, Context, FontFamily, Id, Layout, Rect, RichText, Sense, Stroke, Vec2,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

pub const DEFAULT_SIZE: Vec2 = Vec2::new(1100.0, 700.0);
pub const MIN_SIZE: Vec2 = Vec2::new(820.0, 500.0);
const SIDEBAR_WIDTH: f32 = 306.0;
const FOOTER_HEIGHT: f32 = 108.0;
const HEADER_HEIGHT: f32 = 92.0;
const BUTTON_HEIGHT: f32 = 38.0;
const ROW_HEIGHT: f32 = 36.0;
const MARGIN: i8 = 18;
const BORDER_WIDTH: f32 = 1.0;
const BACKGROUND: Color32 = Color32::from_rgb(246, 248, 252);
const INK: Color32 = Color32::from_rgb(25, 40, 63);
const MUTED: Color32 = Color32::from_rgb(100, 116, 139);
const ACCENT: Color32 = Color32::from_rgb(38, 99, 235);
const BORDER: Color32 = Color32::from_rgb(222, 229, 239);
const GREEN: Color32 = Color32::from_rgb(14, 126, 92);
const ORANGE: Color32 = Color32::from_rgb(185, 90, 12);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filter {
    Changes,
    Ready,
    Problems,
    All,
}
impl Filter {
    fn label(self) -> &'static str {
        match self {
            Self::Changes => "變更與問題",
            Self::Ready => "可執行項目",
            Self::Problems => "衝突與保留",
            Self::All => "全部項目",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewMode {
    Preview,
    Undo,
}
#[derive(Clone, Copy, Debug)]
enum Job {
    Add,
    Scan,
    UndoPreview,
    Apply,
    Undo,
    CheckDictionary,
    DownloadDictionary,
    ActivateDictionary,
    ResetDictionary,
}
enum Completed {
    Add(Vec<Scope>, Vec<String>),
    Preview(Plan, Journal),
    UndoPreview(Plan, Journal, Vec<UndoAction>),
    Finished(usize),
    DictionaryChecked(Release, bool),
    DictionaryStaged(Bundle),
    DictionaryChanged(String),
}
#[derive(Clone, Debug)]
struct Line {
    status: String,
    kind: Kind,
    old: String,
    new: String,
    reason: String,
    tone: Color32,
    executable: bool,
}

#[derive(Clone, Debug)]
pub struct Bounds {
    pub footer: Rect,
    pub scan: Rect,
    pub execute: Rect,
    pub scope_rows: Vec<Rect>,
    pub modes: [Rect; 2],
}
impl Default for Bounds {
    fn default() -> Self {
        Self {
            footer: Rect::NOTHING,
            scan: Rect::NOTHING,
            execute: Rect::NOTHING,
            scope_rows: vec![],
            modes: [Rect::NOTHING; 2],
        }
    }
}

pub struct App {
    pub scopes: Vec<Scope>,
    pub selected: BTreeSet<usize>,
    pub path_focus: bool,
    pub text_focus: bool,
    pub mode: Mode,
    pub plan: Option<Arc<Plan>>,
    pub bounds: Bounds,
    history: PathBuf,
    journal: Option<Journal>,
    view: ViewMode,
    lines: Vec<Line>,
    filtered: Vec<usize>,
    filter: Filter,
    search: String,
    path_input: String,
    last_selected: Option<usize>,
    job: Option<Job>,
    receiver: Option<mpsc::Receiver<std::result::Result<Completed, String>>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<String>>,
    status: String,
    message: Option<(String, String)>,
    confirm: bool,
    backup_ack: bool,
    applied: bool,
    logo: egui::TextureHandle,
    dictionary_dialog: bool,
    dictionary_release: Option<Release>,
    dictionary_candidate: Option<Bundle>,
    dictionary_version: String,
}

pub fn configure(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windows = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()));
    for (name, file) in [("system-ui", "segoeui.ttf"), ("chinese-ui", "msjh.ttc")] {
        if let Ok(bytes) = std::fs::read(windows.join("Fonts").join(file)) {
            fonts
                .font_data
                .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
            if name == "system-ui" {
                fonts
                    .families
                    .entry(FontFamily::Proportional)
                    .or_default()
                    .insert(0, name.to_owned());
            } else {
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push(name.to_owned());
                }
            }
        }
    }
    ctx.set_fonts(fonts);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.override_text_color = Some(INK);
    style.visuals.panel_fill = Color32::WHITE;
    style.visuals.window_fill = Color32::WHITE;
    style.visuals.selection.bg_fill = Color32::from_rgb(226, 236, 255);
    style.visuals.selection.stroke = Stroke::new(BORDER_WIDTH, ACCENT);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(244, 247, 251);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(BORDER_WIDTH, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(232, 239, 252);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(221, 233, 255);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(25.0));
    ctx.set_style(style);
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(BORDER_WIDTH, BORDER))
        .corner_radius(12)
        .inner_margin(14)
}
fn button(text: &str) -> egui::Button<'_> {
    egui::Button::new(text).corner_radius(7)
}

impl App {
    pub fn new(ctx: &Context, history: PathBuf) -> Self {
        configure(ctx);
        let image = image::load_from_memory(include_bytes!("../assets/logo-v2.png"))
            .expect("embedded logo")
            .into_rgba8();
        let logo = ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            ),
            egui::TextureOptions::LINEAR,
        );
        Self {
            scopes: vec![],
            selected: BTreeSet::new(),
            path_focus: false,
            text_focus: false,
            mode: Mode::ZhHant,
            plan: None,
            bounds: Bounds::default(),
            history,
            journal: None,
            view: ViewMode::Preview,
            lines: vec![],
            filtered: vec![],
            filter: Filter::Changes,
            search: String::new(),
            path_input: String::new(),
            last_selected: None,
            job: None,
            receiver: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(String::new())),
            status: "加入檔案或資料夾，先預覽，再確認改名。".to_owned(),
            message: None,
            confirm: false,
            backup_ack: false,
            applied: false,
            logo,
            dictionary_dialog: false,
            dictionary_release: None,
            dictionary_candidate: None,
            dictionary_version: Store::standard()
                .and_then(|s| s.current())
                .map(|x| x.0)
                .unwrap_or_else(|_| "狀態待查核".to_owned()),
        }
    }
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn mode_changed(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.invalidate();
            self.status = "模式已變更，請重新掃描產生預覽。".to_owned();
        }
    }
    fn invalidate(&mut self) {
        self.plan = None;
        self.lines.clear();
        self.filtered.clear();
        self.applied = false;
        self.backup_ack = false;
        self.view = ViewMode::Preview;
    }
    pub fn remove_selected(&mut self) {
        if self.busy() {
            return;
        }
        for &index in self.selected.iter().rev() {
            if index < self.scopes.len() {
                self.scopes.remove(index);
            }
        }
        self.selected.clear();
        self.last_selected = None;
        self.invalidate();
        self.status = "已移除選取路徑；檔案本身沒有刪除。".to_owned();
    }
    pub fn accept_scopes(&mut self, added: Vec<Scope>) {
        let mut duplicate = 0;
        let mut merged = 0;
        let mut count = 0;
        for candidate in added {
            if self.scopes.iter().any(|s| {
                native::key(Path::new(&s.canonical)) == native::key(Path::new(&candidate.canonical))
                    || (s.kind == Kind::Dir
                        && native::contains(
                            Path::new(&s.canonical),
                            Path::new(&candidate.canonical),
                        ))
            }) {
                duplicate += 1;
                continue;
            }
            if candidate.kind == Kind::Dir {
                let before = self.scopes.len();
                self.scopes.retain(|s| {
                    !native::contains(Path::new(&candidate.canonical), Path::new(&s.canonical))
                });
                merged += before - self.scopes.len();
            }
            self.scopes.push(candidate);
            count += 1;
        }
        if count > 0 {
            self.invalidate();
            self.selected.clear();
            self.last_selected = None;
        }
        self.status = format!("加入 {count} 個範圍 · 略過重複 {duplicate} · 整合子範圍 {merged}");
    }
    fn spawn(
        &mut self,
        ctx: &Context,
        job: Job,
        task: impl FnOnce(Arc<AtomicBool>, Arc<Mutex<String>>) -> Result<Completed> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.progress
            .lock()
            .unwrap()
            .clone_from(&"處理中…".to_owned());
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.job = Some(job);
        let cancel = self.cancel.clone();
        let progress = self.progress.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = task(cancel, progress).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    pub fn add_paths(&mut self, ctx: &Context, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.busy() || self.confirm || self.message.is_some() || self.dictionary_dialog {
            self.status = "正在處理，請完成目前步驟後再加入路徑。".to_owned();
            return;
        }
        self.spawn(ctx, Job::Add, move |cancel, progress| {
            let mut added = vec![];
            let mut errors = vec![];
            for path in paths {
                engine::cancelled(&cancel)?;
                *progress.lock().unwrap() = format!("確認路徑 · {}", path.display());
                match engine::scope(&path) {
                    Ok(scope) => added.push(scope),
                    Err(e) => errors.push(format!("{}\n{e:#}", path.display())),
                }
            }
            Ok(Completed::Add(added, errors))
        });
    }
    fn scan(&mut self, ctx: &Context) {
        let paths = self
            .scopes
            .iter()
            .map(|s| PathBuf::from(&s.path))
            .collect::<Vec<_>>();
        let history = self.history.clone();
        let mode = self.mode;
        self.invalidate();
        self.spawn(ctx, Job::Scan, move |cancel, progress| {
            let tick = |s| {
                *progress.lock().unwrap() = s;
            };
            let plan = engine::make_plan_with_mode(&paths, mode, &cancel, &tick)?;
            engine::cancelled(&cancel)?;
            let journal = Journal::create(&history, &plan)?;
            Ok(Completed::Preview(plan, journal))
        });
    }
    fn load_undo(&mut self, ctx: &Context, path: PathBuf) {
        let journal = Journal {
            directory: path.parent().unwrap_or(Path::new(".")).to_owned(),
        };
        self.invalidate();
        self.spawn(ctx, Job::UndoPreview, move |cancel, progress| {
            let tick = |s| {
                *progress.lock().unwrap() = s;
            };
            let (plan, actions) = journal::prepare_undo(&journal, &cancel, &tick)?;
            Ok(Completed::UndoPreview(plan, journal, actions))
        });
    }
    fn execute(&mut self, ctx: &Context) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let Some(journal) = self.journal.clone() else {
            return;
        };
        let view = self.view;
        self.spawn(
            ctx,
            if view == ViewMode::Preview {
                Job::Apply
            } else {
                Job::Undo
            },
            move |cancel, progress| {
                let tick = |s| {
                    *progress.lock().unwrap() = s;
                };
                let count = if view == ViewMode::Preview {
                    journal::apply(&plan, &journal, &cancel, &tick)?
                } else {
                    journal::undo(&journal, &cancel, &tick)?
                };
                Ok(Completed::Finished(count))
            },
        );
    }
    fn poll(&mut self) {
        let messages = self
            .receiver
            .as_ref()
            .map(|rx| rx.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for result in messages {
            let job = self.job.take();
            self.receiver = None;
            match result {
                Ok(Completed::Add(scopes, errors)) => {
                    self.accept_scopes(scopes);
                    if !errors.is_empty() {
                        self.message = Some(("部分路徑未加入".to_owned(), errors.join("\n\n")));
                    }
                }
                Ok(Completed::Preview(plan, journal)) => self.set_preview(plan, journal),
                Ok(Completed::UndoPreview(plan, journal, actions)) => {
                    self.set_undo(plan, journal, actions)
                }
                Ok(Completed::Finished(count)) => {
                    self.applied = true;
                    self.status = format!(
                        "{}完成，已核對 {count} 個項目。",
                        if matches!(job, Some(Job::Undo)) {
                            "復原"
                        } else {
                            "改名"
                        }
                    );
                    for line in &mut self.lines {
                        if line.executable {
                            line.status = "完成".to_owned();
                            line.executable = false;
                            line.tone = GREEN;
                        }
                    }
                    self.refilter();
                    if let Some(plan) = &self.plan {
                        let mapping = if matches!(job, Some(Job::Apply)) {
                            engine::changes(
                                plan,
                                &plan
                                    .rows
                                    .iter()
                                    .filter(|r| r.status == Status::Ready)
                                    .map(|r| r.id)
                                    .collect(),
                            )
                        } else {
                            std::collections::HashMap::new()
                        };
                        for scope in &mut self.scopes {
                            let path = engine::mapped(&scope.path, &mapping);
                            if let Ok(updated) = engine::scope(&path) {
                                *scope = updated;
                            }
                        }
                    }
                }
                Ok(Completed::DictionaryChecked(release, available)) => {
                    self.status = if available {
                        "有 MediaWiki 轉換表更新，可先下載驗證。".to_owned()
                    } else {
                        "目前已是 zhconv 正式版本快照；可重新下載驗證。".to_owned()
                    };
                    self.dictionary_release = Some(release);
                    self.dictionary_dialog = true;
                }
                Ok(Completed::DictionaryStaged(bundle)) => {
                    self.dictionary_candidate = Some(bundle);
                    self.dictionary_dialog = true;
                    self.status = "下載與相容性驗證完成，尚未套用。".to_owned();
                }
                Ok(Completed::DictionaryChanged(version)) => {
                    self.dictionary_version = version;
                    self.dictionary_release = None;
                    self.dictionary_candidate = None;
                    self.invalidate();
                    self.status =
                        "MediaWiki 轉換表已套用，舊設定保留為備份；請重新掃描。".to_owned();
                }
                Err(error) => {
                    if matches!(job, Some(Job::Apply | Job::Undo)) {
                        self.applied = true;
                    }
                    self.status = error.lines().next().unwrap_or("已停止").to_owned();
                    self.message = Some(("作業已停止".to_owned(), error.clone()));
                    if std::fs::create_dir_all(&self.history).is_ok() {
                        use std::io::Write;
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(self.history.join("errors.jsonl"))
                        {
                            let _ = writeln!(
                                file,
                                "{}",
                                serde_json::json!({"time":chrono::Utc::now().to_rfc3339(),"error":error})
                            );
                            let _ = file.sync_all();
                        }
                    }
                }
            }
        }
    }
    pub fn set_preview(&mut self, plan: Plan, journal: Journal) {
        let supported = !plan.legacy && Mode::parse(&plan.mode).is_ok();
        if let Ok(mode) = Mode::parse(&plan.mode) {
            self.mode = mode;
            if supported {
                self.dictionary_version = plan.dictionary_version.clone();
            }
        }
        self.view = ViewMode::Preview;
        self.applied = false;
        self.journal = Some(journal);
        let active = plan
            .rows
            .iter()
            .filter(|r| r.status == Status::Ready)
            .map(|r| r.id)
            .collect();
        let mapping = engine::changes(&plan, &active);
        self.lines = plan
            .rows
            .iter()
            .map(|row| {
                let (status, tone) = match row.status {
                    Status::Ready => ("待改名", GREEN),
                    Status::Conflict => ("同名衝突", ORANGE),
                    Status::Blocked | Status::Excluded => ("保留", MUTED),
                    Status::Unchanged => ("不變", MUTED),
                };
                Line {
                    status: status.to_owned(),
                    kind: row.kind,
                    old: row.path.clone(),
                    new: engine::mapped(&row.path, &mapping)
                        .to_string_lossy()
                        .into_owned(),
                    reason: row.reason.clone(),
                    tone,
                    executable: supported && row.status == Status::Ready,
                }
            })
            .collect();
        self.lines.extend(plan.issues.iter().map(|i| Line {
            status: "無法讀取".to_owned(),
            kind: Kind::File,
            old: i.path.clone(),
            new: String::new(),
            reason: i.reason.clone(),
            tone: ORANGE,
            executable: false,
        }));
        self.plan = Some(Arc::new(plan));
        self.status = if supported {
            "預覽已保存；檢查內容並確認後才會改名。".to_owned()
        } else {
            "舊版紀錄僅供復原；請重新掃描產生預覽。".to_owned()
        };
        self.refilter();
    }
    pub fn set_undo(&mut self, plan: Plan, journal: Journal, actions: Vec<UndoAction>) {
        self.view = ViewMode::Undo;
        self.applied = false;
        if let Ok(mode) = Mode::parse(&plan.mode) {
            self.mode = mode;
        }
        self.journal = Some(journal);
        self.lines = actions
            .into_iter()
            .map(|a| Line {
                status: "待復原".to_owned(),
                kind: plan.rows[a.id].kind,
                old: a.source.to_string_lossy().into_owned(),
                new: a.target.to_string_lossy().into_owned(),
                reason: "依原始紀錄反向復原".to_owned(),
                tone: ACCENT,
                executable: true,
            })
            .collect();
        self.plan = Some(Arc::new(plan));
        self.status = "復原預覽已核對；先上層、再深層。".to_owned();
        self.refilter();
    }
    pub fn displayed_mode(&self) -> String {
        if let Some(plan) = &self.plan
            && (plan.legacy || Mode::parse(&plan.mode).is_err())
        {
            return "舊版紀錄 · 僅供復原".to_owned();
        }
        format!(
            "MediaWiki · {} · {}",
            self.mode.name(),
            self.mode.description()
        )
    }
    pub fn can_execute(&self) -> bool {
        self.lines.iter().any(|r| r.executable)
            && !self.busy()
            && !self.applied
            && (self.view == ViewMode::Undo
                || self
                    .plan
                    .as_ref()
                    .is_some_and(|p| !p.legacy && Mode::parse(&p.mode).is_ok()))
    }
    fn refilter(&mut self) {
        let search = self.search.to_lowercase();
        self.filtered = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let status = match self.filter {
                    Filter::Changes => {
                        r.status != "不變"
                            && !(r.status == "保留" && r.old == r.new && r.reason.contains("系統"))
                    }
                    Filter::Ready => r.executable,
                    Filter::Problems => {
                        matches!(r.status.as_str(), "同名衝突" | "保留" | "無法讀取")
                    }
                    Filter::All => true,
                };
                status
                    && (search.is_empty()
                        || r.old.to_lowercase().contains(&search)
                        || r.new.to_lowercase().contains(&search))
            })
            .map(|(i, _)| i)
            .collect();
    }
    fn keyboard_and_drops(&mut self, ctx: &Context) {
        let drops = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        });
        if !drops.is_empty() {
            ctx.input_mut(|i| i.raw.dropped_files.clear());
            self.add_paths(ctx, drops);
        }
        if self.path_focus
            && !self.text_focus
            && !self.busy()
            && !self.confirm
            && self.message.is_none()
            && !self.dictionary_dialog
        {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)) {
                self.remove_selected();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
                self.selected = (0..self.scopes.len()).collect();
            }
        }
    }
    pub fn draw(&mut self, ctx: &Context) {
        self.poll();
        self.keyboard_and_drops(ctx);
        self.bounds = Bounds::default();
        self.text_focus = false;
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.busy() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.cancel.store(true, Ordering::Relaxed);
            self.status = "正在安全停止，完成目前項目後可關閉視窗。".to_owned();
        }
        // Reserve the bottom actions BEFORE any content. They cannot be pushed below the viewport.
        self.footer(ctx);
        self.header(ctx);
        self.sidebar(ctx);
        self.preview(ctx);
        self.dictionary_window(ctx);
        self.dialogs(ctx);
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                Id::new("drop_hover"),
            ));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0, Color32::from_black_alpha(95));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "放開以加入檔案或資料夾",
                egui::FontId::proportional(26.0),
                Color32::WHITE,
            );
        }
    }
    fn footer(&mut self, ctx: &Context) {
        let frame = egui::Frame::new()
            .fill(Color32::WHITE)
            .inner_margin(egui::Margin::symmetric(MARGIN, 12))
            .stroke(Stroke::new(BORDER_WIDTH, BORDER));
        let shown = egui::TopBottomPanel::bottom("actions")
            .exact_height(FOOTER_HEIGHT)
            .resizable(false)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.busy() {
                        ui.spinner();
                    } else {
                        ui.label(RichText::new("●").color(if self.applied {
                            GREEN
                        } else {
                            ACCENT
                        }));
                    }
                    let status = if self.busy() {
                        self.progress.lock().unwrap().clone()
                    } else {
                        self.status.clone()
                    };
                    ui.add(egui::Label::new(RichText::new(status).small().color(MUTED)).truncate());
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(self.displayed_mode()).strong().small());
                        ui.label(
                            RichText::new("只處理名稱 · 同名不覆蓋 · 保留復原紀錄")
                                .small()
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let executable = self.can_execute();
                        let label = if self.view == ViewMode::Undo {
                            "確認復原"
                        } else {
                            "執行改名"
                        };
                        let execute = ui.add_enabled(
                            executable,
                            egui::Button::new(RichText::new(label).color(Color32::WHITE))
                                .corner_radius(7)
                                .fill(ACCENT)
                                .stroke(Stroke::NONE)
                                .min_size(Vec2::new(130.0, BUTTON_HEIGHT)),
                        );
                        self.bounds.execute = execute.rect;
                        if execute.clicked() {
                            self.confirm = true;
                            self.backup_ack = false;
                        }
                        if self.busy()
                            && ui
                                .add_sized([82.0, BUTTON_HEIGHT], button("停止"))
                                .clicked()
                        {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                        let scan = ui.add_enabled(
                            !self.scopes.is_empty() && !self.busy(),
                            button("掃描預覽").min_size(Vec2::new(112.0, BUTTON_HEIGHT)),
                        );
                        self.bounds.scan = scan.rect;
                        if scan.clicked() {
                            self.scan(ctx);
                        }
                    });
                });
            });
        self.bounds.footer = shown.response.rect;
    }
    fn header(&mut self, ctx: &Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(HEADER_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(MARGIN, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.image((self.logo.id(), egui::vec2(48.0, 48.0)));
                    ui.vertical(|ui| {
                        ui.label(RichText::new("SC2TC-Renamer").size(24.0).strong());
                        ui.label(
                            RichText::new("離線檔名簡轉繁 · MediaWiki")
                                .color(MUTED)
                                .small(),
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("RUST").small().color(ACCENT).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("v{}", engine::APP_VERSION))
                                .small()
                                .color(MUTED),
                        );
                        if ui.add_enabled(!self.busy(), button("復原紀錄")).clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .set_directory(&self.history)
                                .add_filter("改名紀錄", &["json"])
                                .pick_file()
                        {
                            self.load_undo(ctx, path);
                        }
                        if ui.add_enabled(!self.busy(), button("轉換表更新")).clicked() {
                            self.dictionary_dialog = true;
                        }
                    });
                });
            });
    }
    fn sidebar(&mut self, ctx: &Context) {
        egui::SidePanel::left("scopes")
            .exact_width(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(251, 252, 254))
                    .inner_margin(MARGIN),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("處理範圍").strong().size(17.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{} 個", self.scopes.len()))
                                .small()
                                .color(MUTED),
                        );
                    });
                });
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy(), button("＋ 資料夾")).clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder()
                    {
                        self.add_paths(ctx, vec![path]);
                    }
                    if ui.add_enabled(!self.busy(), button("＋ 檔案")).clicked()
                        && let Some(paths) = rfd::FileDialog::new().pick_files()
                    {
                        self.add_paths(ctx, paths);
                    }
                });
                let input = ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .id(Id::new("path_input"))
                        .hint_text("貼上路徑，按 Enter 加入")
                        .desired_width(f32::INFINITY),
                );
                self.text_focus |= input.has_focus();
                if input.lost_focus()
                    && ctx.input(|i| i.key_pressed(egui::Key::Enter))
                    && !self.path_input.trim().is_empty()
                {
                    let path = PathBuf::from(self.path_input.trim().trim_matches('"'));
                    self.path_input.clear();
                    self.add_paths(ctx, vec![path]);
                }
                egui::Frame::new()
                    .fill(Color32::from_rgb(238, 244, 255))
                    .stroke(Stroke::new(BORDER_WIDTH, Color32::from_rgb(210, 223, 249)))
                    .corner_radius(10)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("拖曳檔案或資料夾至視窗")
                                .color(ACCENT)
                                .strong()
                                .small(),
                        );
                        ui.label(
                            RichText::new("支援磁碟根目錄 · 點選後按 Delete 移除")
                                .small()
                                .color(MUTED),
                        );
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Ctrl 多選 · Shift 範圍選取")
                            .small()
                            .color(MUTED),
                    );
                    if ui
                        .add_enabled(
                            !self.selected.is_empty() && !self.busy(),
                            button("移除").small(),
                        )
                        .clicked()
                    {
                        self.remove_selected();
                    }
                });
                ui.separator();
                let entries = self.scopes.clone();
                egui::ScrollArea::vertical()
                    .id_salt("scope_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if entries.is_empty() {
                            ui.add_space(32.0);
                            ui.label(RichText::new("尚未加入路徑").color(MUTED));
                            ui.label(
                                RichText::new("拖放、選擇或貼上路徑即可開始。")
                                    .small()
                                    .color(MUTED),
                            );
                        }
                        for (index, scope) in entries.iter().enumerate() {
                            let selected = self.selected.contains(&index);
                            let frame = egui::Frame::new()
                                .fill(if selected {
                                    Color32::from_rgb(230, 239, 255)
                                } else {
                                    Color32::WHITE
                                })
                                .stroke(Stroke::new(
                                    BORDER_WIDTH,
                                    if selected { ACCENT } else { BORDER },
                                ))
                                .corner_radius(8)
                                .inner_margin(10)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    let name = Path::new(&scope.path)
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or(&scope.path);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(if scope.kind == Kind::Dir {
                                                "▣"
                                            } else {
                                                "▤"
                                            })
                                            .color(ACCENT),
                                        );
                                        ui.add(
                                            egui::Label::new(RichText::new(name).strong())
                                                .truncate(),
                                        );
                                    });
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&scope.path).small().color(MUTED),
                                        )
                                        .truncate(),
                                    );
                                });
                            let response = ui
                                .interact(
                                    frame.response.rect,
                                    Id::new(("scope_row", index)),
                                    Sense::click(),
                                )
                                .on_hover_text(&scope.path);
                            self.bounds.scope_rows.push(response.rect);
                            if response.clicked() && !self.busy() {
                                ctx.memory_mut(|m| {
                                    m.surrender_focus(Id::new("path_input"));
                                    m.surrender_focus(Id::new("preview_search"));
                                });
                                self.path_focus = true;
                                self.text_focus = false;
                                let modifiers = ctx.input(|i| i.modifiers);
                                if modifiers.shift {
                                    if let Some(last) = self.last_selected {
                                        self.selected.extend(index.min(last)..=index.max(last));
                                    } else {
                                        self.selected.insert(index);
                                    }
                                } else if modifiers.ctrl || modifiers.command {
                                    if !self.selected.insert(index) {
                                        self.selected.remove(&index);
                                    }
                                } else {
                                    self.selected.clear();
                                    self.selected.insert(index);
                                }
                                self.last_selected = Some(index);
                            }
                            ui.add_space(3.0);
                        }
                    });
            });
    }
    fn preview(&mut self, ctx: &Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(MARGIN))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if self.view == ViewMode::Undo {
                            "復原預覽"
                        } else {
                            "改名預覽"
                        })
                        .size(20.0)
                        .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                self.plan.is_some()
                                    && !self.busy()
                                    && self.view == ViewMode::Preview,
                                button("匯出 CSV").small(),
                            )
                            .clicked()
                        {
                            self.export();
                        }
                    });
                });
                let old_mode = self.mode;
                if self.view == ViewMode::Preview {
                    ui.add_enabled_ui(!self.busy(), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("轉換模式").small().color(MUTED));
                            for (i, mode) in [Mode::ZhHant, Mode::ZhTw].into_iter().enumerate() {
                                let r = ui.selectable_label(
                                    self.mode == mode,
                                    format!("{}  {}", mode.name(), mode.description()),
                                );
                                self.bounds.modes[i] = r.rect;
                                if r.clicked() {
                                    self.mode = mode;
                                }
                            }
                        });
                    });
                } else {
                    ui.label(RichText::new(self.displayed_mode()).small().color(MUTED));
                }
                if old_mode != self.mode {
                    let new = self.mode;
                    self.mode = old_mode;
                    self.mode_changed(new);
                }
                if ui.available_height() > 290.0 {
                    let stats = [
                        (
                            "可執行",
                            self.lines.iter().filter(|r| r.executable).count(),
                            GREEN,
                        ),
                        (
                            "同名衝突",
                            self.lines.iter().filter(|r| r.status == "同名衝突").count(),
                            ORANGE,
                        ),
                        (
                            "保留／問題",
                            self.lines
                                .iter()
                                .filter(|r| matches!(r.status.as_str(), "保留" | "無法讀取"))
                                .count(),
                            MUTED,
                        ),
                    ];
                    ui.columns(stats.len(), |columns| {
                        for (column, (label, count, color)) in columns.iter_mut().zip(stats) {
                            card().show(column, |ui| {
                                ui.label(RichText::new(label).small().color(MUTED));
                                ui.label(
                                    RichText::new(count.to_string())
                                        .size(25.0)
                                        .strong()
                                        .color(color),
                                );
                            });
                        }
                    });
                }
                ui.add_space(3.0);
                let mut changed = false;
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("filter")
                        .selected_text(self.filter.label())
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for filter in [
                                Filter::Changes,
                                Filter::Ready,
                                Filter::Problems,
                                Filter::All,
                            ] {
                                changed |= ui
                                    .selectable_value(&mut self.filter, filter, filter.label())
                                    .changed();
                            }
                        });
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(Id::new("preview_search"))
                            .hint_text("搜尋名稱或路徑")
                            .desired_width(f32::INFINITY),
                    );
                    changed |= response.changed();
                    self.text_focus |= response.has_focus();
                });
                if changed {
                    self.refilter();
                }
                ui.add_space(4.0);
                let available = ui.available_height();
                card().show(ui, |ui| {
                    ui.set_min_height((available - 30.0).max(60.0));
                    if self.plan.is_none() {
                        ui.add_space(35.0);
                        ui.label(RichText::new("先加入範圍，再掃描預覽").size(19.0).strong());
                        ui.label(
                            RichText::new(
                                "原名稱、新名稱與同名衝突都會列在這裡。\n掃描不會修改任何名稱。",
                            )
                            .color(MUTED),
                        );
                        return;
                    }
                    let width = ui.available_width();
                    let status_width = 82.0;
                    let name_width = ((width - status_width - 24.0) / 2.0).max(110.0);
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [status_width, 24.0],
                            egui::Label::new(RichText::new("狀態").small().color(MUTED)),
                        );
                        ui.add_sized(
                            [name_width, 24.0],
                            egui::Label::new(RichText::new("原名稱").small().color(MUTED)),
                        );
                        ui.add_sized(
                            [name_width, 24.0],
                            egui::Label::new(RichText::new("預計名稱").small().color(MUTED)),
                        );
                    });
                    ui.separator();
                    let total = self.filtered.len();
                    let height = ui.available_height().max(50.0);
                    egui::ScrollArea::vertical()
                        .id_salt("preview_scroll")
                        .max_height(height)
                        .auto_shrink([false, false])
                        .show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                            for index in range {
                                let line = self.lines[self.filtered[index]].clone();
                                let shown = ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [status_width, ROW_HEIGHT],
                                        egui::Label::new(
                                            RichText::new(&line.status).small().color(line.tone),
                                        ),
                                    );
                                    let old = Path::new(&line.old)
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or(&line.old);
                                    let new = Path::new(&line.new)
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or(&line.new);
                                    ui.add_sized(
                                        [name_width, ROW_HEIGHT],
                                        egui::Label::new(old).truncate(),
                                    )
                                    .on_hover_text(&line.old);
                                    ui.add_sized(
                                        [name_width, ROW_HEIGHT],
                                        egui::Label::new(
                                            RichText::new(new).color(if line.executable {
                                                GREEN
                                            } else {
                                                INK
                                            }),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(format!("{}\n{}", line.new, line.reason));
                                });
                                if ui
                                    .interact(
                                        shown.response.rect,
                                        Id::new(("preview_row", index)),
                                        Sense::click(),
                                    )
                                    .double_clicked()
                                {
                                    self.path_focus = false;
                                    self.message = Some((
                                        "項目明細".to_owned(),
                                        format!(
                                            "類型：{}\n\n原路徑\n{}\n\n預計路徑\n{}\n\n{}",
                                            if line.kind == Kind::Dir {
                                                "資料夾"
                                            } else {
                                                "檔案"
                                            },
                                            line.old,
                                            line.new,
                                            line.reason
                                        ),
                                    ));
                                }
                            }
                        });
                    if total == 0 {
                        ui.label(RichText::new("此篩選沒有項目。").color(MUTED));
                    }
                });
            });
    }
    fn export(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("SC2TC-Renamer-preview.csv")
            .add_filter("CSV", &["csv"])
            .save_file()
            && let Some(plan) = &self.plan
        {
            match engine::save_csv(plan, &path) {
                Ok(()) => self.status = format!("已匯出完整預覽：{}", path.display()),
                Err(e) => {
                    self.message = Some((
                        "匯出未完成".to_owned(),
                        format!("不覆蓋既有檔案；請確認路徑或改用其他名稱。\n{e:#}"),
                    ))
                }
            }
        }
    }
    fn dictionary_window(&mut self, ctx: &Context) {
        if !self.dictionary_dialog {
            return;
        }
        let mut check = false;
        let mut download = false;
        let mut activate = false;
        let mut reset = false;
        let mut close = false;
        let response = egui::Modal::new(Id::new("dictionary_update"))
            .frame(card())
            .show(ctx, |ui| {
                ui.set_width((ctx.content_rect().width() - 80.0).min(500.0));
                ui.heading("MediaWiki 轉換表更新");
                ui.label(format!(
                    "目前轉換表：{}　·　zhconv 引擎：{}",
                    self.dictionary_version,
                    crate::converter::ENGINE_VERSION
                ));
                ui.label(
                    RichText::new(
                        "來源：zhconv 正式版本的 MediaWiki 轉換表快照。只在檢查或下載時連線。",
                    )
                    .small()
                    .color(MUTED),
                );
                if self.busy() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(self.progress.lock().unwrap().clone());
                    });
                }
                if let Some(release) = &self.dictionary_release {
                    ui.separator();
                    ui.label(format!("zhconv 正式版本：{}", release.version));
                    ui.label(format!("資源包大小：{} 位元組", release.size));
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("SHA-256：{}", release.digest))
                                .small()
                                .color(MUTED),
                        )
                        .wrap(),
                    );
                }
                if let Some(bundle) = &self.dictionary_candidate {
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "{} 個檔案已驗證，兩種模式載入成功。",
                            bundle.files.len()
                        ))
                        .color(GREEN),
                    );
                    ui.label("尚未套用。套用後舊設定會備份，預覽須重新掃描。");
                }
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.busy(), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_sized([120.0, BUTTON_HEIGHT], button("檢查版本更新"))
                            .clicked()
                        {
                            check = true;
                        }
                        if self.dictionary_release.is_some()
                            && ui
                                .add_sized([120.0, BUTTON_HEIGHT], button("下載並驗證"))
                                .clicked()
                        {
                            download = true;
                        }
                        if self.dictionary_candidate.is_some()
                            && ui
                                .add_sized([110.0, BUTTON_HEIGHT], button("確認套用").fill(ACCENT))
                                .clicked()
                        {
                            activate = true;
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_sized([145.0, BUTTON_HEIGHT], button("回復內附轉換表"))
                            .clicked()
                        {
                            reset = true;
                        }
                        if ui
                            .add_sized([90.0, BUTTON_HEIGHT], button("關閉"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        if close || response.should_close() {
            if self.busy() {
                self.cancel.store(true, Ordering::Relaxed);
            }
            self.dictionary_dialog = false;
        }
        if check {
            self.dictionary_candidate = None;
            self.dictionary_release = None;
            self.spawn(ctx, Job::CheckDictionary, |cancel, _| {
                engine::cancelled(&cancel)?;
                let store = Store::standard()?;
                let release = updater::check_official()?;
                engine::cancelled(&cancel)?;
                let available = updater::available(&store, &release)?;
                store.log(
                    "checked",
                    serde_json::json!({"version":release.version,"available":available}),
                )?;
                Ok(Completed::DictionaryChecked(release, available))
            });
        }
        if download && let Some(release) = self.dictionary_release.clone() {
            self.spawn(ctx, Job::DownloadDictionary, move |cancel, progress| {
                let tick = |s| {
                    *progress.lock().unwrap() = s;
                };
                let bundle =
                    updater::download_and_stage(&Store::standard()?, &release, &cancel, &tick)?;
                Ok(Completed::DictionaryStaged(bundle))
            });
        }
        if activate && let Some(bundle) = self.dictionary_candidate.clone() {
            self.spawn(ctx, Job::ActivateDictionary, move |_, _| {
                Store::standard()?.activate(&bundle)?;
                Ok(Completed::DictionaryChanged(bundle.version))
            });
        }
        if reset {
            self.spawn(ctx, Job::ResetDictionary, |_, _| {
                Store::standard()?.reset_embedded()?;
                Ok(Completed::DictionaryChanged(
                    crate::converter::ENGINE_VERSION.to_owned(),
                ))
            });
        }
    }
    fn dialogs(&mut self, ctx: &Context) {
        if self.confirm {
            let count = self.lines.iter().filter(|r| r.executable).count();
            let mut execute = false;
            let mut close = false;
            let response = egui::Modal::new(Id::new("confirm_operation"))
                .frame(card())
                .show(ctx, |ui| {
                    ui.set_width((ctx.content_rect().width() - 80.0).min(430.0));
                    ui.heading(if self.view == ViewMode::Undo {
                        "確認復原"
                    } else {
                        "確認改名"
                    });
                    ui.label(format!("將處理 {count} 個項目。{}", self.displayed_mode()));
                    ui.label("篩選只影響顯示，執行範圍為完整預覽中的可執行項目。");
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut self.backup_ack,
                        "我已檢查完整預覽，重要文件已有內容備份",
                    );
                    ui.label(
                        RichText::new("名稱紀錄不等於文件內容備份。")
                            .small()
                            .color(MUTED),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized([100.0, BUTTON_HEIGHT], button("返回檢查"))
                            .clicked()
                        {
                            close = true;
                        }
                        if ui
                            .add_enabled(
                                self.backup_ack,
                                button("確認執行")
                                    .fill(ACCENT)
                                    .min_size(Vec2::new(120.0, BUTTON_HEIGHT)),
                            )
                            .clicked()
                        {
                            execute = true;
                        }
                    });
                });
            if response.should_close() {
                close = true;
            }
            if close {
                self.confirm = false;
            }
            if execute {
                self.confirm = false;
                self.execute(ctx);
            }
        }
        if let Some((title, text)) = self.message.clone() {
            let mut close = false;
            let response = egui::Modal::new(Id::new("message"))
                .frame(card())
                .show(ctx, |ui| {
                    ui.set_width((ctx.content_rect().width() - 80.0).min(500.0));
                    ui.heading(title);
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() - 170.0).max(120.0))
                        .show(ui, |ui| {
                            ui.label(text);
                        });
                    if ui
                        .add_sized([110.0, BUTTON_HEIGHT], button("關閉"))
                        .clicked()
                    {
                        close = true;
                    }
                });
            if close || response.should_close() {
                self.message = None;
            }
        }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}
