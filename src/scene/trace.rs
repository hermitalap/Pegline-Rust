//! Optional CSV trace of the paper model, for tuning by numbers instead of
//! screenshots.
//!
//! `PEGLINE_PAPER_TRACE=<path>` opens the file once when the scene is created
//! and writes one row per card per frame. The trace is absent by default, so
//! the shipped app pays for it only when the environment asks for it.

use std::fs::File;
use std::io::{BufWriter, Write};

/// One frame of one card. Kept as a plain struct so the scene builds it on the
/// stack and the formatting lives in this module.
pub(crate) struct TraceRow {
    pub t: f64,
    pub id: u64,
    pub grabbed: bool,
    pub hop: bool,
    pub release: bool,
    pub x: f32,
    pub vx: f32,
    pub drop: f32,
    pub vy: f32,
    pub angle: f32,
    pub omega: f32,
    pub target: f32,
    pub ax: f32,
    pub ay: f32,
    pub g_eff: f32,
    pub wind: f32,
    pub flex: f32,
    pub swap_dir: i32,
}

pub(crate) struct PaperTrace {
    writer: BufWriter<File>,
}

impl PaperTrace {
    /// Opens `PEGLINE_PAPER_TRACE`, if set. A failure is logged and the scene
    /// simply runs without a trace.
    pub(crate) fn open() -> Option<Self> {
        let path = std::env::var_os("PEGLINE_PAPER_TRACE")?;
        if path.is_empty() {
            return None;
        }
        let file = match File::create(&path) {
            Ok(file) => file,
            Err(error) => {
                crate::logging::error(format!(
                    "paper trace: cannot create {}: {error}",
                    std::path::Path::new(&path).display()
                ));
                return None;
            }
        };
        let mut writer = BufWriter::new(file);
        if writeln!(
            writer,
            "t,id,grabbed,hop,release,x,vx,drop,vy,angle,omega,target,ax,ay,g_eff,wind,flex,swap_dir"
        )
        .is_err()
        {
            return None;
        }
        if writer.flush().is_err() {
            return None;
        }
        Some(Self { writer })
    }

    /// Writes and flushes one row. Flushing every frame keeps the trace usable
    /// if the app is stopped by the driver instead of shutting down cleanly.
    pub(crate) fn row(&mut self, row: &TraceRow) -> std::io::Result<()> {
        writeln!(
            self.writer,
            "{:.4},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.4},{:.4},{:.4},{:.2},{:.2},{:.2},{:.3},{:.4},{}",
            row.t,
            row.id,
            u8::from(row.grabbed),
            u8::from(row.hop),
            u8::from(row.release),
            row.x,
            row.vx,
            row.drop,
            row.vy,
            row.angle,
            row.omega,
            row.target,
            row.ax,
            row.ay,
            row.g_eff,
            row.wind,
            row.flex,
            row.swap_dir
        )?;
        self.writer.flush()
    }
}
