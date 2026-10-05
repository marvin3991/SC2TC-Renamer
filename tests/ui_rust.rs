#![cfg(feature = "gui")]
use eframe::egui::{self, Context, Event, Key, Modifiers, RawInput, Rect, Vec2};
use sc2tc_renamer::{
    converter::Mode,
    engine,
    journal::{Journal, UndoAction},
    ui::{App, DEFAULT_SIZE, MIN_SIZE},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use uuid::Uuid;
fn fixture() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/ui-tests")
        .join(Uuid::new_v4().to_string());
    fs::create_dir_all(&path).unwrap();
    path
}
fn frame(ctx: &Context, app: &mut App, size: Vec2, input: Option<RawInput>) {
    let mut input = input.unwrap_or_default();
    input.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, size));
    let _ = ctx.run(input, |ctx| app.draw(ctx));
}
#[test]
fn footer_visible_at_default_minimum_and_scaled_sizes() {
    let ctx = Context::default();
    let mut app = App::new(&ctx, fixture());
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
    let ctx = Context::default();
    let mut app = App::new(&ctx, fixture());
    let input = RawInput {
        dropped_files: vec![egui::DroppedFile {
            path: Some(file.clone()),
            ..Default::default()
        }],
        ..Default::default()
    };
    frame(&ctx, &mut app, DEFAULT_SIZE, Some(input));
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.busy() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        frame(&ctx, &mut app, DEFAULT_SIZE, None);
    }
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
    let ctx = Context::default();
    let mut app = App::new(&ctx, fixture());
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
    let plan = engine::make_plan(&[base], &AtomicBool::new(false), &|_| {}).unwrap();
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let ctx = Context::default();
    let mut app = App::new(&ctx, fixture());
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
    let mut plan = engine::make_plan(
        std::slice::from_ref(&base),
        &AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
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
    let journal = Journal::create(&fixture(), &plan).unwrap();
    let ctx = Context::default();
    let mut app = App::new(&ctx, fixture());
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
