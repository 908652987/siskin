#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod log;
mod model;
mod platform;

use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, Timer, TimerMode};

use model::MessageModel;

// The generated bindings contain `todo!()` stubs for Slint features this app
// does not use (embedded Rust components). Keep the workspace-wide `todo` deny
// for handwritten code and allow it only inside the generated module.
#[allow(clippy::todo)]
mod generated {
    slint::include_modules!();
}

pub use generated::*;

const ROW_COUNT: usize = 100_000;
const BENCH_FRAMES: usize = 300;

fn main() -> Result<(), slint::PlatformError> {
    log::reset();
    log::line("siskin-desktop S3 probe started");
    log::line(&format!(
        "exe={:?} cwd={:?} log={:?}",
        std::env::current_exe(),
        std::env::current_dir(),
        log::path()
    ));

    let ui = AppWindow::new()?;
    ui.set_messages(ModelRc::new(MessageModel::new(ROW_COUNT)));

    ui.on_read_requested({
        let weak = ui.as_weak();
        move |_row| {
            if let Some(ui) = weak.upgrade() {
                open_reading(&ui);
            }
        }
    });

    ui.on_close_reading({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                close_reading(&ui);
            }
        }
    });

    ui.on_start_benchmark({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                start_benchmark(&ui);
            }
        }
    });

    schedule_automation(&ui);

    // Slint has no geometry-change callback for arbitrary elements, so poll the
    // placeholder while the reading pane is open. The timer does nothing while
    // reading is closed, and five wake-ups per second are negligible.
    let pane_timer = Timer::default();
    {
        let weak = ui.as_weak();
        pane_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !ui.get_reading_open() {
                return;
            }
            let rect = platform::PaneRect {
                x: ui.get_pane_x(),
                y: ui.get_pane_y(),
                width: ui.get_pane_width(),
                height: ui.get_pane_height(),
            };
            if let Err(message) = platform::sync(ui.window(), rect) {
                log::line(&format!("reading pane failed: {message}"));
                ui.set_reading_open(false);
                ui.set_status_text(format!("WebView2 失败：{message}").into());
            }
        });
    }

    ui.run()
}

fn open_reading(ui: &AppWindow) {
    ui.set_reading_open(true);
    ui.set_status_text("正在创建 WebView2…".into());
    log::line("reading pane requested");
}

fn close_reading(ui: &AppWindow) {
    platform::close_reading();
    ui.set_reading_open(false);
    ui.set_status_text("正文已关闭，WebView2 已释放".into());
    log::line("reading pane closed");
}

fn start_benchmark(ui: &AppWindow) {
    if ui.get_benchmark_running() {
        return;
    }

    ui.set_benchmark_running(true);
    ui.set_scroll_y(0.0);
    ui.set_status_text("滚动基准运行中（300 帧离屏渲染）…".into());
    log::line("scroll benchmark started");

    let mut frame_times = Vec::with_capacity(BENCH_FRAMES);
    for index in 0..BENCH_FRAMES {
        let offset = (index as f32 * 13.0) % 4_000.0;
        ui.set_scroll_y(offset);
        let start = Instant::now();
        // The software renderer does not support rendering notifiers, so time
        // an off-screen render of the scrolled list instead. It covers layout,
        // row recycling and painting, which dominate software scrolling.
        let _ = ui.window().take_snapshot();
        frame_times.push(start.elapsed().as_secs_f32() * 1000.0);
    }

    ui.set_benchmark_running(false);
    let summary = summarize(&mut frame_times);
    ui.set_status_text(summary.as_str().into());
    log::line(&summary);
}

/// Unattended runs used by `spike/scripts/measure-windows-process.ps1`:
/// `SISKIN_S3_AUTOMATION=bench` starts the scroll benchmark, `=reading` opens
/// the WebView2 pane and closes it after `SISKIN_S3_READING_SECONDS`.
fn schedule_automation(ui: &AppWindow) {
    match std::env::var("SISKIN_S3_AUTOMATION").ok().as_deref() {
        Some("bench") => {
            let weak = ui.as_weak();
            Timer::single_shot(Duration::from_secs(1), move || {
                if let Some(ui) = weak.upgrade() {
                    start_benchmark(&ui);
                }
            });
        }
        Some("reading") => {
            let seconds = std::env::var("SISKIN_S3_READING_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(15);
            let weak = ui.as_weak();
            Timer::single_shot(Duration::from_millis(700), move || {
                if let Some(ui) = weak.upgrade() {
                    open_reading(&ui);
                }
            });
            let weak = ui.as_weak();
            Timer::single_shot(Duration::from_secs(seconds), move || {
                if let Some(ui) = weak.upgrade() {
                    close_reading(&ui);
                    ui.set_status_text("automation: reading closed".into());
                }
            });
        }
        _ => {}
    }
}

fn summarize(frame_times: &mut [f32]) -> String {
    if frame_times.is_empty() {
        return "滚动基准：没有采样到帧".to_string();
    }

    let total: f32 = frame_times.iter().sum();
    let average = total / frame_times.len() as f32;
    frame_times.sort_by(f32::total_cmp);
    let p95 = frame_times[(frame_times.len() * 95 / 100).min(frame_times.len() - 1)];
    let fps = if average > 0.0 { 1000.0 / average } else { 0.0 };

    format!(
        "滚动基准：{fps:.1} fps（离屏渲染均值 {average:.2} ms，p95 {p95:.2} ms，{} 帧）",
        frame_times.len()
    )
}
