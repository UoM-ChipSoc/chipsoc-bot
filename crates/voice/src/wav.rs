//! 16 kHz mono WAV, what speech-to-text engines take: a 48 kHz → 16 kHz
//! downsampler and a writer.

use std::io::{self, Write};
use std::path::Path;

/// The sample rate transcription engines are given.
pub const RATE_16K: u32 = 16_000;

/// Taps in the low-pass filter run before keeping every third sample.
const TAPS: usize = 63;

/// 48 kHz in, 16 kHz out: low-pass (windowed sinc, cut off below 8 kHz),
/// then every third sample. Streams: feed it any number of samples at a time.
pub struct Downsampler {
    coeffs: Vec<f32>,
    history: Vec<f32>,
    /// Where the next sample goes in `history` (a ring).
    at: usize,
    /// Samples in since the last one out.
    phase: usize,
}

impl Default for Downsampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Downsampler {
    pub fn new() -> Self {
        let cutoff = 7_000.0 / 48_000.0;
        let mid = (TAPS - 1) as f32 / 2.0;
        let mut coeffs: Vec<f32> = (0..TAPS)
            .map(|i| {
                let x = i as f32 - mid;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x)
                };
                let window = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (TAPS - 1) as f32).cos();
                sinc * window
            })
            .collect();
        let sum: f32 = coeffs.iter().sum();
        for c in &mut coeffs {
            *c /= sum;
        }
        Self { coeffs, history: vec![0.0; TAPS], at: 0, phase: 0 }
    }

    /// Add 48 kHz samples; the 16 kHz ones they make are appended to `out`.
    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &s in input {
            self.history[self.at] = s as f32;
            self.at = (self.at + 1) % TAPS;
            self.phase += 1;
            if self.phase == 3 {
                self.phase = 0;
                let mut acc = 0.0f32;
                for (k, c) in self.coeffs.iter().enumerate() {
                    acc += c * self.history[(self.at + k) % TAPS];
                }
                out.push(acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            }
        }
    }
}

/// `samples` (16 kHz mono) as a WAV file.
pub fn write(path: &Path, samples: &[i16]) -> io::Result<()> {
    let mut f = io::BufWriter::new(std::fs::File::create(path)?);
    let data = (samples.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&RATE_16K.to_le_bytes())?;
    f.write_all(&(RATE_16K * 2).to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(s: &[i16]) -> f64 {
        (s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
    }

    fn through(hz: f32) -> Vec<i16> {
        let input: Vec<i16> =
            (0..48_000).map(|i| ((i as f32 * hz * std::f32::consts::TAU / 48_000.0).sin() * 10_000.0) as i16).collect();
        let mut d = Downsampler::new();
        let mut out = Vec::new();
        for c in input.chunks(960) {
            d.push(c, &mut out);
        }
        out
    }

    #[test]
    fn speech_passes_and_what_would_alias_is_cut() {
        let low = through(1_000.0);
        assert_eq!(low.len(), 16_000, "a third as many samples");
        let r = rms(&low[100..]);
        assert!((r - 7071.0).abs() < 500.0, "1 kHz keeps its level: {r}");
        let high = through(12_000.0);
        assert!(rms(&high[100..]) < 300.0, "12 kHz is filtered out: {}", rms(&high[100..]));
    }

    #[test]
    fn wav_header_says_what_follows() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.wav");
        write(&p, &[1, 2, 3]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 6);
    }
}
