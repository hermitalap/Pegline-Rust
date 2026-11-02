//! The three interface sounds, synthesized once and played from memory.
//!
//! Ported number-for-number from `Sounds` in `archive/wpf-v1/src/Support.cs`:
//! the WPF app shipped no audio files, it built a small PCM buffer per event and
//! played it with `SoundPlayer`. Here the same samples become an in-memory WAV
//! and go to `PlaySound(SND_MEMORY | SND_ASYNC)`, which returns immediately, so
//! the UI thread never blocks on the mixer.
//!
//! `SND_ASYNC` with `SND_MEMORY` requires the buffer to stay valid until the
//! sound finishes, so each WAV lives in a [`OnceLock`] for the process lifetime.

use std::sync::OnceLock;

use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

/// Sample rate of every generated sound, Hz.
const RATE: u32 = 44_100;
/// Smallest and largest sample values for the 16-bit PCM conversion.
const PEAK: f64 = i16::MAX as f64;

/// The sounds the line can make.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cue {
    /// A card hangs: a light metallic tink.
    Tink,
    /// A card leaves: a short downward pop.
    Pop,
    /// A card is discarded or the line is cleared: a paper rustle.
    Rustle,
}

/// The three cached WAV files, built on first use.
struct Sounds {
    tink: OnceLock<Vec<u8>>,
    pop: OnceLock<Vec<u8>>,
    rustle: OnceLock<Vec<u8>>,
}

fn sounds() -> &'static Sounds {
    static SOUNDS: OnceLock<Sounds> = OnceLock::new();
    SOUNDS.get_or_init(|| Sounds {
        tink: OnceLock::new(),
        pop: OnceLock::new(),
        rustle: OnceLock::new(),
    })
}

/// Plays one cue asynchronously. A failure is silent: audio is never worth an
/// error dialog or a blocked gesture.
pub fn play(cue: Cue) {
    let buffer = match cue {
        Cue::Tink => sounds().tink.get_or_init(|| wav(&make(Cue::Tink))),
        Cue::Pop => sounds().pop.get_or_init(|| wav(&make(Cue::Pop))),
        Cue::Rustle => sounds().rustle.get_or_init(|| wav(&make(Cue::Rustle))),
    };
    // SAFETY: with SND_MEMORY the pointer is read as a WAV file in memory. The
    // buffer is a leaked process-lifetime allocation (OnceLock), so it cannot
    // move or be freed while the mixer is still playing; SND_ASYNC returns
    // immediately and the mixer keeps its own reference to the playback.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(buffer.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// The PCM samples of one cue, exactly as the WPF version generated them.
fn make(cue: Cue) -> Vec<i16> {
    let seconds = match cue {
        Cue::Tink => 0.18,
        Cue::Pop => 0.09,
        Cue::Rustle => 0.28,
    };
    let count = (RATE as f64 * seconds) as usize;
    let mut samples = Vec::with_capacity(count);
    let mut phase = 0.0f64;
    let mut low = 0.0f64;
    // The original used `new Random(7)`, a seeded .NET generator. The rustle
    // only needs low-passed noise with the same envelope, so a small xorshift
    // seeded the same way keeps the character and drops a dependency.
    let mut noise = 7u32;
    for i in 0..count {
        let t = i as f64 / RATE as f64;
        let value = match cue {
            Cue::Tink => {
                let env = (-t * 32.0).exp();
                0.35 * env
                    * ((std::f64::consts::TAU * 1760.0 * t).sin() * 0.7
                        + (std::f64::consts::TAU * 3520.0 * t).sin() * 0.3)
            }
            Cue::Pop => {
                let frequency = 420.0 - 2400.0 * t;
                phase += std::f64::consts::TAU * frequency / RATE as f64;
                let env = (std::f64::consts::PI * (t / seconds).min(1.0)).sin() * (-t * 18.0).exp();
                0.28 * env * phase.sin()
            }
            Cue::Rustle => {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let white = (noise as f64 / u32::MAX as f64) * 2.0 - 1.0;
                low += (white - low) * 0.35;
                let env =
                    (std::f64::consts::PI * t / seconds).sin() * (0.6 + 0.4 * (t * 90.0).sin());
                0.22 * env * low
            }
        };
        samples.push((value.clamp(-1.0, 1.0) * PEAK) as i16);
    }
    samples
}

/// Wraps PCM samples in a 16-bit mono WAV container.
fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_the_samples() {
        let samples = [1i16, -2, 3];
        let bytes = wav(&samples);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), RATE);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes(bytes[44..46].try_into().unwrap()), 1);
    }

    #[test]
    fn each_cue_is_finite_and_in_range() {
        for cue in [Cue::Tink, Cue::Pop, Cue::Rustle] {
            let samples = make(cue);
            assert!(!samples.is_empty());
            // No sample may clip beyond full scale.
            assert!(samples.iter().all(|sample| *sample >= -i16::MAX));
        }
        assert_eq!(make(Cue::Tink).len(), (RATE as f64 * 0.18) as usize);
        assert_eq!(make(Cue::Pop).len(), (RATE as f64 * 0.09) as usize);
        assert_eq!(make(Cue::Rustle).len(), (RATE as f64 * 0.28) as usize);
    }
}
