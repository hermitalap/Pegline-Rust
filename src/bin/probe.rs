//! `probe`: the measurement binary.
//!
//! It drives the real renderer and the real scene on a real window, so the
//! numbers are the app's numbers:
//!
//! ```text
//! probe --mode=line   --seconds=10   # 6 cards, continuous wind
//! probe --mode=idle   --seconds=10   # 6 cards, settled, no frame loop
//! probe --mode=flight --seconds=5    # the real flight window
//! probe --mode=static --seconds=10   # one frame, then no presents
//! ```
//!
//! Output is one `PROBE ...` line per run: fps, p50/p95/max frame interval,
//! draw time, private memory and working set.
//!
//! This is developer tooling: it is not packaged and has no UI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, MA_NOACTIVATE, SW_SHOWNOACTIVATE, ShowWindow,
    WM_ERASEBKGND, WM_MOUSEACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use pegline::layout;
use pegline::render::{Present, Renderer};
use pegline::scene::{Command, Scene};
use pegline::types::{CardId, CardSpec, FrameSnapshot, ImageData, RectI};
use pegline::win32;

const CLASS_NAME: PCWSTR = w!("PeglineProbe");
const WINDOW_TITLE: PCWSTR = w!("Pegline Probe");

fn main() {
    let config = match Config::parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("probe: {error}");
            std::process::exit(2);
        }
    };
    win32::ensure_dpi_awareness();
    if let Err(error) = pegline::window::ComGuard::apartment() {
        eprintln!("probe: {error}");
        std::process::exit(1);
    }

    let result = match config.mode {
        Mode::Line | Mode::Idle | Mode::Static | Mode::Clear | Mode::Primitive => run_line(&config),
        Mode::Flight => run_flight(&config),
    };
    match result {
        Ok(line) => println!("{line}"),
        Err(error) => {
            eprintln!("probe: {error}");
            std::process::exit(1);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Line,
    Idle,
    Static,
    Flight,
    Clear,
    Primitive,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Line => "line",
            Mode::Idle => "idle",
            Mode::Static => "static",
            Mode::Clear => "clear",
            Mode::Primitive => "primitive",
            Mode::Flight => "flight",
        }
    }
}

struct Config {
    mode: Mode,
    seconds: f64,
    cards: usize,
    repeat: usize,
    prim: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Line,
            seconds: 10.0,
            cards: 6,
            repeat: 1,
            prim: "card".to_string(),
        }
    }
}

impl Config {
    fn parse<I: Iterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut config = Self::default();
        for arg in args {
            let (key, value) = arg
                .split_once('=')
                .ok_or_else(|| format!("expected --key=value, got {arg}"))?;
            match key {
                "--mode" => {
                    config.mode = match value {
                        "line" | "animate" => Mode::Line,
                        "idle" => Mode::Idle,
                        "static" => Mode::Static,
                        "flight" => Mode::Flight,
                        "clear" => Mode::Clear,
                        "prim" => Mode::Primitive,
                        other => return Err(format!("unknown mode {other}")),
                    }
                }
                "--seconds" => {
                    config.seconds = value.parse().map_err(|_| "bad --seconds".to_string())?
                }
                "--cards" => config.cards = value.parse().map_err(|_| "bad --cards".to_string())?,
                "--repeat" => {
                    config.repeat = value.parse().map_err(|_| "bad --repeat".to_string())?
                }
                "--prim" => config.prim = value.to_string(),
                other => return Err(format!("unknown switch {other}")),
            }
        }
        Ok(config)
    }
}

fn run_line(config: &Config) -> Result<String, String> {
    let monitor = primary_monitor()?;
    let width_px = monitor.work.width().max(1) as u32;
    let height_px = ((layout::PANEL_HEIGHT * monitor.scale()).ceil() as i32).max(1) as u32;
    let hwnd = create_window(monitor.work.left, monitor.work.top, width_px, height_px)?;

    let mut renderer = Renderer::new(hwnd, width_px, height_px, monitor.dpi)?;
    let snapshot = Arc::new(Mutex::new(FrameSnapshot::default()));
    let mut scene = Scene::new(
        width_px,
        monitor.dpi,
        renderer.actions_layout(),
        renderer.copied_height(),
        snapshot,
    );
    scene.apply(Command::SetVisible(true), &renderer, 0.0);
    scene.apply(Command::Reveal(true), &renderer, 0.0);
    for index in 0..config.cards.max(1) {
        let (w, h) = match index % 3 {
            0 => (480u32, 270u32),
            1 => (270, 480),
            _ => (400, 300),
        };
        let image = gradient_image(w, h, index as u8);
        scene.apply(
            Command::AddCard(CardSpec {
                id: index as CardId + 1,
                card_size: layout::card_size(w as f32, h as f32),
                tilt: (index as f32 * 1.7) - 2.5,
                arriving: false,
                image: Some(image),
            }),
            &renderer,
            0.0,
        );
    }
    // SAFETY: the window was created by this thread and is shown without
    // activation so it does not steal focus from whatever is in front.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    let start = win32::now_seconds();
    let mut last = start;
    let mut stamps: Vec<f64> = Vec::with_capacity(65_536);
    let mut draws: Vec<f64> = Vec::with_capacity(65_536);
    let mut frames = 0u64;
    let mut next_gust = 0.0f64;

    loop {
        let now = win32::now_seconds();
        let elapsed = now - start;
        if elapsed >= config.seconds {
            break;
        }
        let settled = elapsed > 2.0;
        if config.mode == Mode::Idle && settled {
            // No frame loop at all: this is the steady state the app sits in.
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let dt = ((now - last) as f32).clamp(0.0, 0.05);
        last = now;
        if config.mode == Mode::Line && now >= next_gust {
            scene.apply(Command::Gust, &renderer, now);
            next_gust = now + 1.2;
        }
        scene.update(dt, now);

        let draw_start = win32::now_seconds();
        renderer.begin()?;
        if config.mode == Mode::Primitive {
            draw_primitive(&mut renderer, &scene, &config.prim);
        } else if config.mode != Mode::Clear {
            scene.draw(&mut renderer);
        }
        renderer.end()?;
        let draw_ms = (win32::now_seconds() - draw_start) * 1000.0;
        match renderer.present(true) {
            Present::DeviceLost => return Err("device lost".to_string()),
            Present::Ok | Present::Retry => {}
        }
        frames += 1;
        draws.push(draw_ms);
        stamps.push(now);
        if config.mode == Mode::Static {
            break;
        }
    }

    let deltas: Vec<f64> = stamps
        .windows(2)
        .map(|window| window[1] - window[0])
        .filter(|delta| *delta > 0.0)
        .collect();

    if config.mode == Mode::Idle {
        let remaining = config.seconds - (win32::now_seconds() - start);
        if remaining > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(remaining));
        }
    }

    let (private_mb, ws_mb) = process_memory_mb();
    let summary = Summary::new(&deltas);
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);

    // SAFETY: the window belongs to this thread and the renderer/scene are
    // dropped before the destroy.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    Ok(format!(
        "PROBE mode={} width={} height={} dpi={} cards={} frames={} fps={:.2} p50={:.2} p95={:.2} max={:.2} draw_p50={:.3} draw_p95={:.3} draw_max={:.3} private={:.1}MB ws={:.1}MB exe={}B",
        config.mode.as_str(),
        width_px,
        height_px,
        monitor.dpi,
        config.cards,
        frames,
        summary.fps,
        summary.p50,
        summary.p95,
        summary.max,
        percentile(&draws, 0.50),
        percentile(&draws, 0.95),
        draws.iter().cloned().fold(0.0, f64::max),
        private_mb,
        ws_mb,
        exe
    ))
}

fn run_flight(config: &Config) -> Result<String, String> {
    let monitor = primary_monitor()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut flight = pegline::window::flight::Flight::create(Box::new(move |event| {
        let _ = tx.send(event);
    }))?;

    let image = gradient_image(1600, 1000, 7);
    let from = RectI::new(
        monitor.work.right - 1700,
        monitor.work.bottom - 1100,
        monitor.work.right - 100,
        monitor.work.bottom - 100,
    );
    let to = RectI::new(
        monitor.work.left + monitor.work.width() / 2 - 150,
        monitor.work.top + 90,
        monitor.work.left + monitor.work.width() / 2 + 150,
        monitor.work.top + 290,
    );

    let mut total_frames = 0u32;
    let mut flight_seconds = 0.0f32;
    let mut draw_max = 0.0f32;
    for repeat in 0..config.repeat.max(1) {
        if !flight.fly(repeat as u64 + 1, image.clone(), from, to, monitor.dpi) {
            return Err("flight did not start".to_string());
        }
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(pegline::window::flight::FlightEvent::Landed {
                frames,
                elapsed,
                draw_max_ms,
                ..
            }) => {
                total_frames += frames;
                flight_seconds += elapsed;
                draw_max = draw_max.max(draw_max_ms);
            }
            Ok(pegline::window::flight::FlightEvent::Finished(_)) => {
                return Err("flight finished before landing".to_string());
            }
            Err(error) => return Err(format!("flight timeout: {error}")),
        }
        // Wait for the landing fade before the next take-off.
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }
    flight.hide();
    let (private_mb, ws_mb) = process_memory_mb();
    let fps = if flight_seconds > 0.0 {
        total_frames as f64 / flight_seconds as f64
    } else {
        0.0
    };
    flight.shutdown();
    let _ = config.seconds;

    Ok(format!(
        "PROBE mode=flight rep={} frames={} seconds={:.3} fps={:.2} draw_max={:.3} private={:.1}MB ws={:.1}MB",
        config.repeat, total_frames, flight_seconds, fps, draw_max, private_mb, ws_mb
    ))
}

fn primary_monitor() -> Result<win32::Monitor, String> {
    let monitors = win32::monitors();
    monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or_else(|| monitors.first())
        .copied()
        .ok_or_else(|| "no monitors".to_string())
}

fn create_window(x: i32, y: i32, width: u32, height: u32) -> Result<HWND, String> {
    // SAFETY: the class name and procedure are static; the class is registered
    // on this thread before the window is created.
    let instance = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW: {e}"))?
    };
    let class = WNDCLASSW {
        style: windows::Win32::UI::WindowsAndMessaging::CS_HREDRAW
            | windows::Win32::UI::WindowsAndMessaging::CS_VREDRAW,
        lpfnWndProc: Some(probe_wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(instance.0),
        hIcon: Default::default(),
        hCursor: Default::default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    // SAFETY: registering a class for this process; an existing class is fine.
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::RegisterClassW(&class);
    }
    // SAFETY: the class is registered and the window is only shown by the
    // caller.
    unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_TOPMOST
                | WS_EX_LAYERED
                | WS_EX_TRANSPARENT,
            CLASS_NAME,
            WINDOW_TITLE,
            WS_POPUP,
            x,
            y,
            width as i32,
            height as i32,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))
}

fn gradient_image(width: u32, height: u32, seed: u8) -> ImageData {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            pixels[i] = 30u8
                .wrapping_add((x * 200 / width.max(1)) as u8)
                .wrapping_add(seed.wrapping_mul(7));
            pixels[i + 1] = 60u8
                .wrapping_add((y * 140 / height.max(1)) as u8)
                .wrapping_add(seed.wrapping_mul(5));
            pixels[i + 2] = 140 + (seed as u32 * 11 % 90) as u8;
            pixels[i + 3] = 255;
        }
    }
    ImageData::new(width, height, pixels)
}

struct Summary {
    fps: f64,
    p50: f64,
    p95: f64,
    max: f64,
}

impl Summary {
    fn new(intervals: &[f64]) -> Self {
        let mean = if intervals.is_empty() {
            0.0
        } else {
            intervals.iter().sum::<f64>() / intervals.len() as f64
        };
        Self {
            fps: if mean > 0.0 { 1.0 / mean } else { 0.0 },
            p50: percentile(intervals, 0.50) * 1000.0,
            p95: percentile(intervals, 0.95) * 1000.0,
            max: intervals.iter().cloned().fold(0.0, f64::max) * 1000.0,
        }
    }
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn process_memory_mb() -> (f64, f64) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: `counters` is a live struct with its size passed in.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_err() {
        return (0.0, 0.0);
    }
    (
        counters.PrivateUsage as f64 / (1024.0 * 1024.0),
        counters.WorkingSetSize as f64 / (1024.0 * 1024.0),
    )
}

/// SAFETY: reads the message parameters only; the window is destroyed on the
/// same thread.
unsafe extern "system" fn probe_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    // SAFETY: default processing for messages we do not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Draws one primitive kind, for the memory bisection in `--mode=primitive`.
fn draw_primitive(renderer: &mut Renderer, scene: &Scene, kind: &str) {
    let size = (144.0, 98.7);
    let rect = pegline::types::RectF::from_xywh(1300.0, 60.0, size.0, size.1);
    let _ = scene;
    match kind {
        "nothing" => {}
        "rope" => renderer.draw_rope(3008.0),
        "shadow" => renderer.draw_shadow(rect, 0.18),
        "fill" => renderer.fill_round(rect, 16.0, pegline::render::Brush::Glass),
        "text" => renderer.draw_hint(1504.0, 120.0),
        "card" => {
            renderer.draw_shadow(rect, 0.18);
            renderer.fill_round(rect, 16.0, pegline::render::Brush::Glass);
            renderer.stroke_card(rect, 16.0, 0.75);
        }
        _ => {}
    }
}
