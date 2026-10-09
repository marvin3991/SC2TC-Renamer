#![cfg(feature = "gui")]
use eframe::egui::{
    self, Context, Event, FontId, FullOutput, Key, Modifiers, RawInput, Rect, Vec2,
    ViewportCommand, ViewportEvent, ViewportId, ViewportInfo,
};
use sc2tc_renamer::{
    converter::Mode,
    diagnostics, engine,
    journal::{self, Journal, UndoAction},
    native,
    ui::{App, DEFAULT_SIZE, MIN_SIZE},
    updater::Store,
};
use std::{
    ffi::OsString,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Once, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Upper bound for a background job in these tests.
const JOB_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Other test processes share the cross-process operation lock, which `apply`
/// and `undo` take without waiting.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// 600 attempts × 100 ms: wait up to one minute for those holders.
const LOCK_RETRY_LIMIT: u32 = 600;

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

static DICTIONARY_STORE: Once = Once::new();
/// Points `Store::standard()` at an empty store under work/, so the tests use
/// the embedded table and never the developer's applied dictionary.
fn isolate_dictionary_store() {
    DICTIONARY_STORE.call_once(|| {
        Store::override_standard_root(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("work/ui_rust-dictionary-store"),
        )
        .unwrap();
    });
}

/// A synthetic directory under work/ that is removed when the test passes and
/// kept for inspection when it fails.
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
    isolate_dictionary_store();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/ui-tests")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&path).unwrap();
    Fixture(path)
}
fn run_frame(ctx: &Context, app: &mut App, size: Vec2, input: Option<RawInput>) -> FullOutput {
    let mut input = input.unwrap_or_default();
    input.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, size));
    ctx.run(input, |ctx| app.draw(ctx))
}
fn frame(ctx: &Context, app: &mut App, size: Vec2, input: Option<RawInput>) {
    let _ = run_frame(ctx, app, size, input);
}
fn wait_idle(ctx: &Context, app: &mut App) {
    let deadline = Instant::now() + JOB_TIMEOUT;
    while app.busy() && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
        frame(ctx, app, DEFAULT_SIZE, None);
    }
    assert!(!app.busy(), "background job did not finish in time");
}
fn close_request() -> RawInput {
    let mut input = RawInput::default();
    input.viewports.insert(
        ViewportId::ROOT,
        ViewportInfo {
            events: vec![ViewportEvent::Close],
            ..Default::default()
        },
    );
    input
}
fn cancels_close(output: &FullOutput) -> bool {
    output
        .viewport_output
        .get(&ViewportId::ROOT)
        .is_some_and(|v| v.commands.contains(&ViewportCommand::CancelClose))
}
fn drop_input(path: &Path) -> RawInput {
    RawInput {
        dropped_files: vec![egui::DroppedFile {
            path: Some(path.to_owned()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn footer_visible_at_default_minimum_and_scaled_sizes() {
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    assert_eq!(app.mode, Mode::ZhHant);
    for size in [DEFAULT_SIZE, MIN_SIZE, Vec2::new(900.0, 560.0)] {
        for _ in 0..3 {
            frame(&ctx, &mut app, size, None);
        }
        let viewport = Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(
            viewport.contains_rect(app.bounds.execute),
            "execute clipped at {size:?}"
        );
        assert!(
            viewport.contains_rect(app.bounds.scan),
            "scan clipped at {size:?}"
        );
        for mode in app.bounds.modes {
            assert!(viewport.contains_rect(mode), "mode clipped at {size:?}");
        }
    }
}
#[test]
fn external_drop_adds_actual_paths_and_delete_removes_only_list() {
    let base = fixture();
    let file = base.join("软件.txt");
    fs::write(&file, b"must stay").unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(drop_input(&file)));
    wait_idle(&ctx, &mut app);
    assert_eq!(app.scopes.len(), 1);
    app.selected.insert(0);
    app.path_focus = true;
    app.text_focus = false;
    let input = RawInput {
        events: vec![Event::Key {
            key: Key::Delete,
            physical_key: Some(Key::Delete),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        ..Default::default()
    };
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(input));
    assert!(app.scopes.is_empty());
    assert_eq!(fs::read(file).unwrap(), b"must stay");
}
#[test]
fn delete_in_text_input_does_not_remove_paths() {
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    let root = fixture();
    app.accept_scopes(vec![engine::scope(&root).unwrap()]);
    app.selected.insert(0);
    app.path_focus = true;
    app.text_focus = true;
    let input = RawInput {
        events: vec![Event::Key {
            key: Key::Delete,
            physical_key: Some(Key::Delete),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        ..Default::default()
    };
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(input));
    assert_eq!(app.scopes.len(), 1);
}
#[test]
fn changing_mode_invalidates_previous_preview() {
    let base = fixture();
    fs::write(base.join("软件.txt"), b"source").unwrap();
    let plan = engine::make_plan(&[base.to_path_buf()], &AtomicBool::new(false), &|_| {}).unwrap();
    let records = fixture();
    let journal = Journal::create(&records, &plan).unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.set_preview(plan, journal);
    assert!(app.plan.is_some());
    app.mode_changed(Mode::ZhTw);
    assert!(app.plan.is_none());
    assert_eq!(app.mode, Mode::ZhTw);
}

#[test]
fn legacy_plan_hides_old_engine_name_and_allows_only_undo() {
    let base = fixture();
    let source = base.join("软件.txt");
    fs::write(&source, b"source").unwrap();
    let mut plan =
        engine::make_plan(&[base.to_path_buf()], &AtomicBool::new(false), &|_| {}).unwrap();
    plan.mode = "s2twp.json".to_owned();
    plan.dictionary_version = "1.4.2".to_owned();
    let row = plan
        .rows
        .iter()
        .find(|r| r.path.ends_with("软件.txt"))
        .unwrap();
    let action = UndoAction {
        id: row.id,
        source: base.join(&row.new),
        target: source,
    };
    let records = fixture();
    let journal = Journal::create(&records, &plan).unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.mode_changed(Mode::ZhTw);
    app.set_preview(plan.clone(), journal.clone());
    assert_eq!(app.displayed_mode(), "舊版紀錄 · 僅供復原");
    assert!(!app.can_execute());
    assert_eq!(app.mode, Mode::ZhTw);
    app.set_undo(plan, journal, vec![action]);
    assert_eq!(app.displayed_mode(), "舊版紀錄 · 僅供復原");
    assert!(app.can_execute());
    for size in [DEFAULT_SIZE, MIN_SIZE] {
        frame(&ctx, &mut app, size, None);
        let viewport = Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(viewport.contains_rect(app.bounds.execute));
    }
}

#[test]
fn worker_panic_ends_job_and_allows_closing() {
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.spawn_failing_job_for_test(&ctx);
    assert!(app.busy());
    wait_idle(&ctx, &mut app);
    assert!(
        app.message_text().is_some_and(|t| t.contains("異常結束")),
        "{:?}",
        app.message_text()
    );
    let errors = fs::read_to_string(history.join("errors.jsonl")).unwrap();
    assert!(errors.contains("異常結束"));
    let output = run_frame(&ctx, &mut app, DEFAULT_SIZE, Some(close_request()));
    assert!(!cancels_close(&output));
}

#[test]
fn close_while_busy_is_cancelled_with_visible_notice() {
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    let (release, gate) = mpsc::channel();
    app.spawn_waiting_job_for_test(&ctx, gate);
    let output = run_frame(&ctx, &mut app, DEFAULT_SIZE, Some(close_request()));
    assert!(cancels_close(&output));
    assert!(app.notice().is_some_and(|n| n.contains("正在安全停止")));
    release.send(()).unwrap();
    wait_idle(&ctx, &mut app);
    assert!(app.notice().is_none());
}

// ui-main-3: a path dropped while the window waits for a safe stop adds
// nothing and leaves the stopping notice in place.
#[test]
fn drop_while_stopping_keeps_the_stopping_notice() {
    let base = fixture();
    let file = base.join("软件.txt");
    fs::write(&file, b"source").unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    let (release, gate) = mpsc::channel();
    app.spawn_waiting_job_for_test(&ctx, gate);
    let output = run_frame(&ctx, &mut app, DEFAULT_SIZE, Some(close_request()));
    assert!(cancels_close(&output));
    assert!(
        app.notice().is_some_and(|n| n.contains("正在安全停止")),
        "{:?}",
        app.notice()
    );
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(drop_input(&file)));
    assert!(app.busy());
    assert!(
        app.notice().is_some_and(|n| n.contains("正在安全停止")),
        "{:?}",
        app.notice()
    );
    assert!(app.scopes.is_empty());
    release.send(()).unwrap();
    wait_idle(&ctx, &mut app);
    assert!(app.scopes.is_empty());
}

// Control for ui-main-3: without a pending stop, a drop during a job is
// refused with the "processing" notice.
#[test]
fn drop_while_busy_reports_processing_notice() {
    let base = fixture();
    let file = base.join("软件.txt");
    fs::write(&file, b"source").unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    let (release, gate) = mpsc::channel();
    app.spawn_waiting_job_for_test(&ctx, gate);
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(drop_input(&file)));
    assert!(app.busy());
    assert!(
        app.notice().is_some_and(|n| n.contains("正在處理")),
        "{:?}",
        app.notice()
    );
    assert!(app.scopes.is_empty());
    release.send(()).unwrap();
    wait_idle(&ctx, &mut app);
    assert!(app.scopes.is_empty());
}

#[test]
fn input_while_busy_is_refused_without_losing_typed_path() {
    let base = fixture();
    let file = base.join("软件.txt");
    fs::write(&file, b"source").unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    let (release, gate) = mpsc::channel();
    app.spawn_waiting_job_for_test(&ctx, gate);
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(drop_input(&file)));
    assert!(app.busy());
    assert!(app.scopes.is_empty());
    assert!(app.notice().is_some_and(|n| !n.is_empty()));
    let typed = file.display().to_string();
    app.path_input = typed.clone();
    app.submit_path_input(&ctx);
    assert_eq!(app.path_input, typed);
    assert!(app.scopes.is_empty());
    assert!(app.notice().is_some_and(|n| !n.is_empty()));
    release.send(()).unwrap();
    wait_idle(&ctx, &mut app);
    assert!(app.scopes.is_empty());
    assert_eq!(app.path_input, typed);
    app.submit_path_input(&ctx);
    assert!(app.path_input.is_empty());
    wait_idle(&ctx, &mut app);
    assert_eq!(app.scopes.len(), 1);
}

#[test]
fn file_scope_follows_apply_and_undo() {
    let _lock = exclusive();
    let base = fixture();
    let source = base.join("软件.txt");
    fs::write(&source, b"source").unwrap();
    let cancel = AtomicBool::new(false);
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.accept_scopes(vec![engine::scope(&source).unwrap()]);
    let plan = engine::make_plan(std::slice::from_ref(&source), &cancel, &|_| {}).unwrap();
    let records = fixture();
    let journal = Journal::create(&records, &plan).unwrap();
    app.set_preview(plan.clone(), journal.clone());
    let renamed = journal::apply(&plan, &journal, &cancel, &|_| {}).unwrap();
    assert_eq!(renamed, 1);
    app.finish_for_test(false, renamed);
    assert_eq!(app.scopes.len(), 1);
    assert!(
        app.scopes[0].path.ends_with("軟件.txt"),
        "{}",
        app.scopes[0].path
    );
    let restored = journal::undo(&journal, &cancel, &|_| {}).unwrap();
    app.finish_for_test(true, restored);
    assert_eq!(app.scopes.len(), 1);
    assert!(
        app.scopes[0].path.ends_with("软件.txt"),
        "{}",
        app.scopes[0].path
    );
    fs::remove_file(&source).unwrap();
    app.finish_for_test(true, 0);
    assert!(app.scopes.is_empty());
    assert!(app.status().contains("已不存在"), "{}", app.status());
}

#[test]
fn resetting_dictionary_reports_embedded_table() {
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.dictionary_reset_for_test();
    assert!(app.status().contains("內附"), "{}", app.status());
    assert!(!app.status().contains("已套用"), "{}", app.status());
}

#[test]
fn undo_view_shows_record_mode_without_changing_selection() {
    let base = fixture();
    fs::write(base.join("软件.txt"), b"source").unwrap();
    let plan = engine::make_plan(&[base.to_path_buf()], &AtomicBool::new(false), &|_| {}).unwrap();
    assert_eq!(plan.mode, "zh-Hant.json");
    let records = fixture();
    let journal = Journal::create(&records, &plan).unwrap();
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    app.mode_changed(Mode::ZhTw);
    app.set_undo(plan, journal, vec![]);
    assert_eq!(app.mode, Mode::ZhTw);
    assert!(
        app.displayed_mode().contains("zh-Hant"),
        "{}",
        app.displayed_mode()
    );
    app.remove_selected();
    assert!(
        app.displayed_mode().contains("zh-TW"),
        "{}",
        app.displayed_mode()
    );
}

#[test]
fn chinese_font_renders_ui_text() {
    let fonts = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()))
        .join("Fonts");
    if !["msjh.ttc", "msyh.ttc"]
        .iter()
        .any(|name| fonts.join(name).is_file())
    {
        eprintln!("skipped: no Microsoft JhengHei or YaHei font installed");
        return;
    }
    let history = fixture();
    let ctx = Context::default();
    let mut app = App::new(&ctx, history.to_path_buf());
    frame(&ctx, &mut app, DEFAULT_SIZE, None);
    assert!(app.fonts_loaded());
    assert!(ctx.fonts_mut(|f| f.has_glyphs(&FontId::proportional(15.0), "繁體轉換")));
}

#[test]
fn failure_report_only_for_flag_with_output_path() {
    let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(diagnostics::failure_report_path(&args(&[])), None);
    assert_eq!(
        diagnostics::failure_report_path(&args(&["--self-test"])),
        None
    );
    assert_eq!(
        diagnostics::failure_report_path(&args(&["报告.docx", "合同.docx"])),
        None
    );
    assert_eq!(
        diagnostics::failure_report_path(&args(&["--self-test", ""])),
        None
    );
    for flag in diagnostics::CLI_FLAGS {
        assert_eq!(
            diagnostics::failure_report_path(&args(&[flag, "work\\check-1"])),
            Some(PathBuf::from("work\\check-1.failure.json")),
            "{flag}"
        );
    }
    // ui-main-4: the report suffix is appended, never replacing an extension.
    for (items, expected) in [
        (
            ["--check-dictionary-update", "work\\update.json"],
            "work\\update.json.failure.json",
        ),
        (
            ["--self-test", "work\\check-1.2"],
            "work\\check-1.2.failure.json",
        ),
    ] {
        assert_eq!(
            diagnostics::failure_report_path(&args(&items)),
            Some(PathBuf::from(expected)),
            "{items:?}"
        );
    }
}

#[test]
fn window_size_comparison_uses_tolerance() {
    let expected = [MIN_SIZE.x, MIN_SIZE.y];
    let tolerance = diagnostics::SIZE_TOLERANCE;
    assert!(diagnostics::size_matches(expected, expected, tolerance));
    assert!(diagnostics::size_matches(
        [MIN_SIZE.x + tolerance, MIN_SIZE.y - tolerance],
        expected,
        tolerance
    ));
    assert!(!diagnostics::size_matches(
        [MIN_SIZE.x - tolerance - 0.5, MIN_SIZE.y],
        expected,
        tolerance
    ));
    assert!(!diagnostics::size_matches(
        [MIN_SIZE.x, MIN_SIZE.y + tolerance + 0.5],
        expected,
        tolerance
    ));
}

#[test]
fn fixture_is_removed_on_success_and_kept_on_failure() {
    let path = {
        let kept = fixture();
        fs::write(kept.join("hidden.txt"), b"synthetic").unwrap();
        kept.to_path_buf()
    };
    assert!(!path.exists());
    let failed = std::panic::catch_unwind(|| {
        let kept = fixture();
        let path = kept.to_path_buf();
        std::panic::panic_any(path);
    })
    .unwrap_err()
    .downcast::<PathBuf>()
    .unwrap();
    assert!(failed.is_dir());
    fs::remove_dir_all(*failed).unwrap();
}
