//! Turning files and the clipboard into cards: restore, watch, dedupe, hand off.
//!
//! The watcher and the clipboard path both end here. The interesting part is
//! not the IO (that is on workers) but the twin window: Windows writes the same
//! screenshot twice, once into the app's inbox and once into the user's
//! Screenshots folder, and the two have to fold into one card.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    CLIPBOARD_DUPLICATE_SECONDS, Event, Item, PEEK_SECONDS, Recent, TWIN_SECONDS, file_name,
    pseudo_random,
};
use crate::capture::wic;
use crate::layout;
use crate::logging;
use crate::notes;
use crate::settings;
use crate::sound::Cue;
use crate::types::{CardId, ImageData, RectI};
use crate::win32;
use crate::window;
use crate::window::flight::PinMode;

impl super::App {
    pub(super) fn restore_pegged(&mut self) {
        let paths: Vec<PathBuf> = settings::read_pegged()
            .into_iter()
            .filter(|path| path.exists() && (wic::is_image_path(path) || notes::is_note(path)))
            .collect();
        self.capture.restore_pending = paths.len();
        for path in paths {
            if !path.exists() || !(wic::is_image_path(&path) || notes::is_note(&path)) {
                continue;
            }
            let id = self.line.next_id;
            self.line.next_id += 1;
            let tx = self.events_tx.clone();
            let thread_path = path.clone();
            let thumb_max = self.thumb_max();
            let note = notes::is_note(&path);
            let color = self.settings.note_color(&file_name(&path));
            let _ = std::thread::Builder::new()
                .name("pegline-restore".to_string())
                .spawn(move || {
                    let (thumb, text) = if note {
                        let text = notes::read(&thread_path);
                        let image = text.as_deref().and_then(|text| notes::render(text, color));
                        (image, text)
                    } else {
                        (wic::decode_scaled(&thread_path, thumb_max).ok(), None)
                    };
                    if thumb.is_none() {
                        logging::error(format!("restore {}", thread_path.display()));
                    }
                    let size_px = if note {
                        (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32)
                    } else {
                        // The source pixels, not the thumbnail's: the card's
                        // aspect, the annotation flight and the editor layout
                        // all use this size.
                        wic::pixel_size(&thread_path)
                            .ok()
                            .or_else(|| thumb.as_ref().map(|thumb| (thumb.width, thumb.height)))
                            .unwrap_or((0, 0))
                    };
                    let _ = tx.send(Event::RestoreReady {
                        id,
                        path: thread_path.clone(),
                        size_px,
                        thumb,
                        note,
                        text,
                        color,
                    });
                    window::wake();
                });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_restore_ready(
        &mut self,
        id: CardId,
        path: PathBuf,
        size_px: (u32, u32),
        thumb: Option<ImageData>,
        note: bool,
        text: Option<String>,
        color: usize,
    ) {
        self.capture.restore_pending = self.capture.restore_pending.saturating_sub(1);
        let Some(thumb) = thumb else {
            if self.capture.restore_pending == 0 {
                self.save_pegged();
            }
            return;
        };
        if self.line.items.len() >= layout::capacity(100_000.0) {
            return;
        }
        let own = path.starts_with(win32::inbox_dir()) || notes::is_note(&path);
        self.add_item(
            id, path, own, size_px, thumb, None, false, false, note, color, text,
        );
    }

    pub(super) fn on_capture_file(&mut self, path: PathBuf) {
        if !wic::is_image_path(&path) {
            return;
        }
        let now = win32::now_seconds();
        // Our own annotation save lands in the watched folder; it is hung by
        // the editor callback, not by the watcher.
        if self
            .capture
            .recently_saved
            .iter()
            .any(|(saved, at)| saved == &path && now - at < 2.0)
        {
            return;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        if metadata.len() == 0 {
            return;
        }
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
            .map(|age| age.as_secs_f64() < 3.0)
            .unwrap_or(false);
        let own = path.starts_with(win32::inbox_dir());
        let area = self.guess_area(&path, fresh);
        self.spawn_capture(path, own, area);
    }

    pub(super) fn spawn_capture(&mut self, path: PathBuf, own: bool, area: Option<RectI>) {
        let now = win32::now_seconds();
        self.prune_recent(now);
        // Candidates must be recorded before the worker starts so two captures
        // that land together can still see each other.
        let candidates: Vec<(PathBuf, bool)> = self
            .capture
            .recent
            .iter()
            .filter(|recent| recent.path != path)
            .map(|recent| (recent.path.clone(), recent.own))
            .collect();
        self.capture.recent.retain(|recent| recent.path != path);
        self.capture.recent.push(Recent {
            at: now,
            path: path.clone(),
            own,
        });

        let tx = self.events_tx.clone();
        let thread_path = path.clone();
        let want_flight = area.is_some();
        let thumb_max = self.thumb_max();
        let _ = std::thread::Builder::new()
            .name("pegline-decode".to_string())
            .spawn(move || {
                let thumb = match wic::decode_scaled(&thread_path, thumb_max) {
                    Ok(thumb) => thumb,
                    Err(error) => {
                        logging::error(format!("decode {}: {error}", thread_path.display()));
                        return;
                    }
                };
                let size_px = wic::pixel_size(&thread_path).unwrap_or((thumb.width, thumb.height));
                // Only a capture with a known origin flies, so only then is the
                // big texture worth decoding.
                let flight_image = if want_flight {
                    wic::decode_scaled(&thread_path, crate::capture::FLIGHT_MAX).ok()
                } else {
                    None
                };
                // Twin: the same picture saved by Windows and caught here, one
                // of them long-lived and one of them a fresh inbox copy. The
                // comparison decodes both at 2048 px, so it stays off the UI.
                let mut twin = None;
                for (candidate, candidate_own) in candidates {
                    if candidate_own != own
                        && candidate.exists()
                        && wic::same_picture(&thread_path, &candidate).unwrap_or(false)
                    {
                        twin = Some(candidate);
                        break;
                    }
                }
                let _ = tx.send(Event::CaptureReady {
                    path: thread_path,
                    own,
                    size_px,
                    thumb,
                    area,
                    flight_image,
                    twin,
                    clip_hash: None,
                });
                window::wake();
            });
    }

    /// Forgets captures older than the twin window.
    pub(super) fn prune_recent(&mut self, now: f64) {
        self.capture
            .recent
            .retain(|recent| now - recent.at < TWIN_SECONDS);
    }

    /// Folds a capture into the card that already shows its twin. The
    /// Windows-saved file always wins; the inbox copy is deleted, as in the WPF
    /// app. Returns false when the twin is not on the line yet: the caller then
    /// hangs this copy normally, and the twin folds into it when it arrives.
    pub(super) fn resolve_twin(&mut self, path: &Path, own: bool, twin: &Path) -> bool {
        logging::trace(format!(
            "twin: {} + {} (new own={own})",
            path.display(),
            twin.display()
        ));
        if !self.line.items.iter().any(|item| item.path == twin) {
            return false;
        }
        self.capture
            .recent
            .retain(|recent| recent.path.as_path() != path && recent.path.as_path() != twin);
        let now = win32::now_seconds();
        if own {
            // New arrival is our inbox copy; the external file stays.
            if let Err(error) = std::fs::remove_file(path) {
                logging::error(format!("twin remove {}: {error}", path.display()));
            }
            self.capture.recent.push(Recent {
                at: now,
                path: twin.to_path_buf(),
                own: false,
            });
            return true;
        }
        // New arrival is the Windows-saved copy: repoint the existing card and
        // delete the inbox file it was showing.
        if let Some(item) = self.line.items.iter_mut().find(|item| item.path == twin) {
            item.path = path.to_path_buf();
            item.own = false;
            item.last_write = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
        }
        if let Err(error) = std::fs::remove_file(twin) {
            logging::error(format!("twin remove {}: {error}", twin.display()));
        }
        self.save_pegged();
        self.capture.recent.push(Recent {
            at: now,
            path: path.to_path_buf(),
            own: false,
        });
        true
    }

    /// Whether a card already on the line is the other half of this capture:
    /// the other owner, the same picture size, caught inside the twin window
    /// and with the same thumbnail.
    pub(super) fn is_twin_candidate(
        item: &Item,
        own: bool,
        size_px: (u32, u32),
        thumb_hash: u64,
        now: f64,
    ) -> bool {
        !item.note
            && item.own != own
            && item.size_px == size_px
            && item.captured_at > 0.0
            && now - item.captured_at < TWIN_SECONDS
            && Self::image_hash(&item.thumb) == thumb_hash
    }

    /// FNV-1a over a sampled view of the pixels. The sampling matches the WPF
    /// catcher (every 4th row, 3-byte steps) so repeated clipboard copies of the
    /// same screenshot hash the same way.
    pub(super) fn image_hash(image: &ImageData) -> u64 {
        let mut hash: u64 = 14_695_981_039_346_656_037;
        hash ^= (image.width as u64) * 31 + image.height as u64;
        let stride = image.width as usize * 4;
        for (row, y) in (0..image.height as usize).step_by(4).enumerate() {
            let _ = row;
            let start = y * stride;
            let end = (start + stride).min(image.bgra.len());
            let mut index = start;
            while index < end {
                hash ^= image.bgra[index] as u64;
                hash = hash.wrapping_mul(1_099_511_628_211);
                index += 3;
            }
        }
        hash
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_capture_ready(
        &mut self,
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        area: Option<RectI>,
        flight_image: Option<ImageData>,
        twin: Option<PathBuf>,
        clip_hash: Option<u64>,
        now: f64,
    ) {
        if let Some(hash) = clip_hash {
            if hash == self.capture.last_clip_hash
                && now - self.capture.last_clip_at < CLIPBOARD_DUPLICATE_SECONDS
            {
                logging::trace("clipboard duplicate ignored");
                let _ = std::fs::remove_file(&path);
                self.capture.recent.retain(|recent| recent.path != path);
                return;
            }
            self.capture.last_clip_hash = hash;
            self.capture.last_clip_at = now;
        }
        // The capture the editor is already working on must not hang twice.
        if self
            .sessions
            .editing
            .as_ref()
            .is_some_and(|editing| editing.path == path)
        {
            return;
        }
        if self.line.items.iter().any(|item| item.path == path) {
            return;
        }
        // The worker's twin search can miss when both workers decide at the
        // same moment: the clipboard decode and the folder watcher race, and
        // each sees only the captures registered before it started. The line
        // is the tie-breaker — both CaptureReady events are handled in order
        // on this thread, so the second one always sees the first card. The
        // thumbnails are the same picture at the same size, which makes a
        // hash match conclusive (and the card that arrived first is kept).
        let thumb_hash = Self::image_hash(&thumb);
        let line_twin = self
            .line
            .items
            .iter()
            .find(|item| Self::is_twin_candidate(item, own, size_px, thumb_hash, now))
            .map(|item| item.path.clone());
        let twin = line_twin.or(twin);
        if let Some(twin) = twin {
            if self.resolve_twin(&path, own, &twin) {
                return;
            }
        }
        let id = self.line.next_id;
        self.line.next_id += 1;
        let mut flying = false;
        if let (Some(area), Some(image), Some(dpi)) = (area, flight_image, self.screen_dpi()) {
            let card_size = layout::card_size(size_px.0 as f32, size_px.1 as f32);
            let index = self.line.items.len();
            if let Some(end) = self.flight_target_px(index, index + 1, card_size) {
                // A burst of captures must not queue flights: show the old card
                // at its slot and let the flight window switch to the new one.
                self.release_flying_card();
                if let Some(flight) = self.windows.ensure_flight(&self.events_tx) {
                    // Same tilt `add_item` gives the real card, and the row
                    // glide it will perform after landing. The flight window
                    // mirrors both during the hand-off.
                    let tilt = pseudo_random(id) * 5.0 - 2.5;
                    let slide_dip = if index == 0 {
                        0.0
                    } else {
                        -layout::SPACING * 0.5
                    };
                    flying = flight.fly(id, image, area, end, tilt, slide_dip, PinMode::Full, dpi);
                    if flying {
                        self.windows.flight_idle_at = None;
                        self.book.flying_card = Some(id);
                        self.book.flying_rehang = false;
                    }
                }
            }
        }
        self.add_item(
            id,
            path.clone(),
            own,
            size_px,
            thumb,
            area,
            true,
            flying,
            false,
            0,
            None,
        );
        if let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) {
            item.captured_at = now;
        }
        self.capture.recent.retain(|recent| recent.path != path);
        self.capture.recent.push(Recent { at: now, path, own });
        self.sound(Cue::Tink);
        self.present();
        self.reveal(false, PEEK_SECONDS);
        // `present`/`reveal` may put the line above the flight window when the
        // strip was hidden. The flight image has to stay on top of the real
        // card: the latter is already opaque underneath it during the hand-off.
        if flying {
            if let Some(flight) = self.windows.flight.as_ref() {
                win32::set_topmost(flight.hwnd());
            }
        }
    }
}
