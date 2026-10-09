#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{Context, Result};
use eframe::egui;
use sc2tc_renamer::{diagnostics, engine, ui};
use std::{ffi::OsString, path::PathBuf, time::Instant};
use windows_sys::Win32::{
    Foundation::RECT,
    UI::{
        HiDpi::GetDpiForSystem,
        WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW},
    },
};

fn window_size() -> egui::Vec2 {
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut rect as *mut RECT).cast(), 0) } != 0
    {
        const BASE_DPI: f32 = 96.0;
        const WORK_AREA_FRACTION: f32 = 0.88;
        let dpi = unsafe { GetDpiForSystem() }.max(BASE_DPI as u32) as f32;
        let width = (rect.right - rect.left) as f32 * BASE_DPI / dpi * WORK_AREA_FRACTION;
        let height = (rect.bottom - rect.top) as f32 * BASE_DPI / dpi * WORK_AREA_FRACTION;
        return egui::vec2(
            ui::DEFAULT_SIZE.x.min(width),
            ui::DEFAULT_SIZE.y.min(height),
        );
    }
    ui::DEFAULT_SIZE
}

struct Smoke {
    app: ui::App,
    out: PathBuf,
    stage: usize,
    frames: usize,
    start: Instant,
}
impl eframe::App for Smoke {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.app.draw(ctx);
        self.frames += 1;
        let screenshots = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        });
        for image in screenshots {
            let file = if self.stage == 1 {
                "default-window.png"
            } else {
                "minimum-window.png"
            };
            let pixels = image
                .pixels
                .iter()
                .flat_map(|p| p.to_array())
                .collect::<Vec<_>>();
            let result = image::save_buffer(
                self.out.join(file),
                &pixels,
                image.size[0] as u32,
                image.size[1] as u32,
                image::ColorType::Rgba8,
            );
            if let Err(error) = result {
                let _ = diagnostics::write_json(
                    &self.out.join("ui-failure.json"),
                    &serde_json::json!({"error":error.to_string()}),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            if self.stage == 1 {
                self.stage = 2;
                self.frames = 0;
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(ui::MIN_SIZE));
            } else {
                let bounds = &self.app.bounds;
                let viewport = ctx.content_rect();
                let visible =
                    viewport.contains_rect(bounds.execute) && viewport.contains_rect(bounds.scan);
                let minimum = [viewport.width(), viewport.height()];
                // A small high-DPI screen can clamp the window below MIN_SIZE,
                // so the minimum capture only counts when the size really matches.
                let captured = diagnostics::size_matches(
                    minimum,
                    [ui::MIN_SIZE.x, ui::MIN_SIZE.y],
                    diagnostics::SIZE_TOLERANCE,
                );
                let _ = diagnostics::write_json(
                    &self.out.join("ui-verification.json"),
                    &serde_json::json!({"engine":"MediaWiki","mode":self.app.mode.name(),"native_window_created":true,"default_and_minimum_captured":captured,"footer_buttons_visible":visible,"fonts_loaded":self.app.fonts_loaded(),"minimum_size":minimum,"expected_minimum_size":[ui::MIN_SIZE.x,ui::MIN_SIZE.y]}),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                self.stage = 4;
            }
        }
        const SETTLE_FRAMES: usize = 12;
        if (self.stage == 0 || self.stage == 2) && self.frames >= SETTLE_FRAMES {
            self.stage += 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        const TIMEOUT_SECONDS: u64 = 30;
        if self.start.elapsed().as_secs() > TIMEOUT_SECONDS {
            let _ = diagnostics::write_json(
                &self.out.join("ui-timeout.json"),
                &serde_json::json!({"stage":self.stage}),
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

fn run(args: &[OsString]) -> Result<()> {
    if args.first().is_some_and(|s| s == "--self-test-update") {
        let path = PathBuf::from(args.get(1).context("請指定尚不存在的更新測試資料夾")?);
        let path = sc2tc_renamer::native::absolute(&path)?;
        std::fs::create_dir(&path)?;
        let store = sc2tc_renamer::updater::Store {
            root: path.join("dictionary-store"),
        };
        let release = sc2tc_renamer::updater::check_official()?;
        let bundle = sc2tc_renamer::updater::download_and_stage(
            &store,
            &release,
            &std::sync::atomic::AtomicBool::new(false),
            &|_| {},
        )?;
        store.activate(&bundle)?;
        let activated = store.active()?.is_some();
        store.reset_embedded()?;
        diagnostics::write_json(
            &path.join("update-verification.json"),
            &serde_json::json!({"engine":"MediaWiki","source":"zhconv stable release snapshot","official_version":release.version,"sha256":release.digest,"files":bundle.files.len(),"download_verified":true,"modes_loaded":["zh-Hant","zh-TW"],"activated":activated,"embedded_restored":store.active()?.is_none(),"fixture":path}),
        )?;
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|s| s == "--check-dictionary-update")
    {
        let path = PathBuf::from(args.get(1).context("請指定驗證結果 JSON 路徑")?);
        let release = sc2tc_renamer::updater::check_official()?;
        diagnostics::write_json(&path, &serde_json::to_value(release)?)?;
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--self-test") {
        let path = PathBuf::from(args.get(1).context("請指定尚不存在的測試資料夾")?);
        diagnostics::self_test(&path)?;
        return Ok(());
    }
    let smoke = args.first().is_some_and(|s| s == "--ui-self-check");
    let out = if smoke {
        Some(PathBuf::from(
            args.get(1).context("請指定尚不存在的 UI 測試資料夾")?,
        ))
    } else {
        None
    };
    let size = if smoke {
        ui::DEFAULT_SIZE
    } else {
        window_size()
    };
    let minimum = egui::vec2(ui::MIN_SIZE.x.min(size.x), ui::MIN_SIZE.y.min(size.y));
    let logo = image::load_from_memory(include_bytes!("../assets/logo-v2.png"))?.into_rgba8();
    let icon = egui::IconData {
        rgba: logo.as_raw().clone(),
        width: logo.width(),
        height: logo.height(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size(minimum)
            .with_drag_and_drop(true)
            .with_icon(icon),
        centered: true,
        persist_window: false,
        ..Default::default()
    };
    let history = engine::history_root()?;
    let fixture = if let Some(path) = &out {
        Some(diagnostics::ui_fixture(path)?)
    } else {
        None
    };
    eframe::run_native(
        "SC2TC-Renamer · MediaWiki",
        options,
        Box::new(move |cc| {
            let mut app = ui::App::new(&cc.egui_ctx, history);
            if let Some((plan, journal)) = fixture {
                app.accept_scopes(plan.scopes.clone());
                app.set_preview(plan, journal);
            }
            if let Some(out) = out {
                Ok(Box::new(Smoke {
                    app,
                    out,
                    stage: 0,
                    frames: 0,
                    start: Instant::now(),
                }))
            } else {
                Ok(Box::new(app))
            }
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if smoke {
        let output = PathBuf::from(args.get(1).context("請指定尚不存在的 UI 測試資料夾")?);
        let verification = output.join("ui-verification.json");
        let bytes = match std::fs::read(&verification) {
            Ok(bytes) => bytes,
            Err(error) => {
                // The window closes early on a screenshot failure or timeout;
                // carry those reports into the error instead of only "not found".
                let details = ["ui-failure.json", "ui-timeout.json"]
                    .iter()
                    .filter_map(|name| {
                        std::fs::read_to_string(output.join(name))
                            .ok()
                            .map(|text| format!("\n{name}：{}", text.trim()))
                    })
                    .collect::<String>();
                anyhow::bail!(
                    "讀不到 UI 驗證結果 {}：{error}{details}",
                    verification.display()
                );
            }
        };
        let report: serde_json::Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            report["footer_buttons_visible"] == true,
            "底部按鈕可見性檢查失敗"
        );
        anyhow::ensure!(
            report["default_and_minimum_captured"] == true,
            "最小視窗尺寸不符：實際 {}，預期 {}",
            report["minimum_size"],
            report["expected_minimum_size"]
        );
    }
    Ok(())
}
fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Err(error) = run(&args) {
        if let Some(report) = diagnostics::failure_report_path(&args) {
            eprintln!("{error:#}");
            if let Some(parent) = report.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(write_error) =
                diagnostics::write_json(&report, &serde_json::json!({"error":format!("{error:#}")}))
            {
                eprintln!("無法寫入失敗報告 {}：{write_error:#}", report.display());
            }
        } else if args.first().is_some_and(diagnostics::is_cli_flag) {
            // A flag without its output path: report on the console only, so
            // unattended scripts are never blocked by a dialog.
            eprintln!("{error:#}");
            std::process::exit(2);
        } else {
            let text = sc2tc_renamer::native::wide(std::ffi::OsStr::new(&format!(
                "無法開啟程式：\n{error:#}"
            )));
            let title = sc2tc_renamer::native::wide(std::ffi::OsStr::new("SC2TC-Renamer"));
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    title.as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}
