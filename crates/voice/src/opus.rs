//! Ogg Opus files (RFC 7845): 48 kHz mono, one 20 ms packet per frame, so a
//! packet's place in the file is its time in the recording.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;

use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use opus2::{Application, Bitrate, Channels, Decoder, Encoder};

use crate::{FRAME, RATE};

/// Bits per second for speech.
const BITRATE: i32 = 32_000;
/// Packets per Ogg page, so at most this much is lost if the bot stops suddenly.
const PAGE: u32 = 50;
/// Silent frames encoded for real before the cached silent packet is reused.
const QUIET_FRAMES: u32 = 5;

/// Counts what's written through it.
struct Counted<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Writes one mono track.
pub struct Writer<W: Write> {
    ogg: PacketWriter<'static, Counted<W>>,
    enc: Encoder,
    serial: u32,
    /// Packets written.
    packets: u64,
    in_page: u32,
    /// Silent frames in a row.
    quiet: u32,
    /// A silent packet, to repeat through long silences without encoding.
    silence: Option<Vec<u8>>,
    zeros: Vec<i16>,
}

impl Writer<BufWriter<File>> {
    /// A new file at `path`.
    pub fn create(path: &Path, serial: u32) -> io::Result<Self> {
        Self::new(BufWriter::new(File::create(path)?), serial)
    }
}

impl<W: Write> Writer<W> {
    /// Start a stream on `w`: its two header packets, each on its own page.
    pub fn new(w: W, serial: u32) -> io::Result<Self> {
        let mut enc = Encoder::new(RATE, Channels::Mono, Application::Voip).map_err(other)?;
        enc.set_bitrate(Bitrate::Bits(BITRATE)).map_err(other)?;
        let pre_skip = u16::try_from(enc.get_lookahead().map_err(other)?).unwrap_or(0);
        let mut ogg = PacketWriter::new(Counted { inner: w, bytes: 0 });
        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(1); // channels
        head.extend(pre_skip.to_le_bytes());
        head.extend(RATE.to_le_bytes());
        head.extend(0i16.to_le_bytes()); // gain
        head.push(0); // mapping family: mono/stereo
        ogg.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)?;
        let vendor = concat!("chipsoc-voice ", env!("CARGO_PKG_VERSION"));
        let mut tags = b"OpusTags".to_vec();
        tags.extend((vendor.len() as u32).to_le_bytes());
        tags.extend(vendor.as_bytes());
        tags.extend(0u32.to_le_bytes());
        ogg.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)?;
        Ok(Self { ogg, enc, serial, packets: 0, in_page: 0, quiet: 0, silence: None, zeros: vec![0; FRAME] })
    }

    /// Add 20 ms: `frame` ([`FRAME`] samples), or silence.
    pub fn push(&mut self, frame: Option<&[i16]>) -> io::Result<()> {
        let packet = match frame {
            Some(f) => {
                self.quiet = 0;
                self.encode(f)?
            }
            None => {
                self.quiet = self.quiet.saturating_add(1);
                match &self.silence {
                    Some(p) if self.quiet > QUIET_FRAMES => p.clone(),
                    _ => {
                        let zeros = std::mem::take(&mut self.zeros);
                        let p = self.encode(&zeros);
                        self.zeros = zeros;
                        let p = p?;
                        if self.quiet == QUIET_FRAMES {
                            self.silence = Some(p.clone());
                        }
                        p
                    }
                }
            }
        };
        self.packets += 1;
        self.in_page += 1;
        let end = if self.in_page >= PAGE {
            self.in_page = 0;
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        self.ogg.write_packet(packet, self.serial, end, self.packets * FRAME as u64)?;
        if matches!(end, PacketWriteEndInfo::EndPage) {
            self.ogg.inner_mut().flush()?;
        }
        Ok(())
    }

    fn encode(&mut self, f: &[i16]) -> io::Result<Vec<u8>> {
        let mut frame = [0i16; FRAME];
        let n = f.len().min(FRAME);
        frame[..n].copy_from_slice(&f[..n]);
        self.enc.encode_vec(&frame, 1500).map_err(other)
    }

    /// Frames written.
    pub fn frames(&self) -> u64 {
        self.packets
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.ogg.inner().bytes
    }

    /// End the stream (a last silent packet marks its end) and flush.
    pub fn finish(mut self) -> io::Result<W> {
        let zeros = vec![0i16; FRAME];
        let last = self.encode(&zeros)?;
        self.packets += 1;
        self.ogg.write_packet(last, self.serial, PacketWriteEndInfo::EndStream, self.packets * FRAME as u64)?;
        let mut c = self.ogg.into_inner();
        c.flush()?;
        Ok(c.inner)
    }
}

/// Decode a track written by [`Writer`], giving each frame's samples in turn
/// (the encoder's delay already taken off). Stops at the end, or at a
/// damaged page (the bot stopped mid-write) with what came before.
pub fn decode(path: &Path, mut each: impl FnMut(&[i16])) -> io::Result<()> {
    let mut r = PacketReader::new(BufReader::new(File::open(path)?));
    let mut dec = Decoder::new(RATE, Channels::Mono).map_err(other)?;
    let mut skip = 0usize;
    let mut out = vec![0i16; FRAME * 6];
    let mut n = 0u64;
    loop {
        let p = match r.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) if n > 2 => {
                tracing::debug!("{}: stopped at a damaged page: {e}", path.display());
                break;
            }
            Err(e) => return Err(other(e)),
        };
        n += 1;
        if p.data.starts_with(b"OpusHead") {
            skip = p.data.get(10..12).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
            continue;
        }
        if p.data.starts_with(b"OpusTags") {
            continue;
        }
        let got = dec.decode(&p.data, &mut out, false).map_err(other)?;
        let drop = skip.min(got);
        skip -= drop;
        if got > drop {
            each(&out[drop..got]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, hz: f32) -> Vec<i16> {
        (0..n).map(|i| ((i as f32 * hz * std::f32::consts::TAU / RATE as f32).sin() * 8000.0) as i16).collect()
    }

    fn energy(s: &[i16]) -> f64 {
        s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / s.len().max(1) as f64
    }

    #[test]
    fn a_track_decodes_to_the_same_timeline() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.ogg");
        let mut w = Writer::create(&p, 1).unwrap();
        // 1 s silence, 1 s tone, 1 s silence.
        for _ in 0..50 {
            w.push(None).unwrap();
        }
        let sine = tone(RATE as usize, 440.0);
        for f in sine.chunks(FRAME) {
            w.push(Some(f)).unwrap();
        }
        for _ in 0..50 {
            w.push(None).unwrap();
        }
        assert_eq!(w.frames(), 150);
        w.finish().unwrap();
        let mut pcm = Vec::new();
        decode(&p, |f| pcm.extend_from_slice(f)).unwrap();
        let second = RATE as usize;
        assert!(pcm.len() >= 3 * second - FRAME && pcm.len() <= 3 * second + FRAME, "{}", pcm.len());
        let (a, b, c) = (&pcm[..second - 2000], &pcm[second + 2000..2 * second - 2000], &pcm[2 * second + 2000..]);
        assert!(energy(a) < 1000.0, "quiet first: {}", energy(a));
        assert!(energy(b) > 1e6, "the tone where it was: {}", energy(b));
        assert!(energy(c) < 1000.0, "quiet after: {}", energy(c));
        // A long silence costs little.
        let size = std::fs::metadata(&p).unwrap().len();
        assert!(size < 12_000, "{size} bytes");
    }

    #[test]
    fn a_track_cut_short_still_decodes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.ogg");
        let mut w = Writer::create(&p, 1).unwrap();
        for f in tone(RATE as usize * 2, 300.0).chunks(FRAME) {
            w.push(Some(f)).unwrap();
        }
        // Never finished: as if the bot stopped. What was flushed is there.
        drop(w);
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() - 7]).unwrap();
        let mut n = 0;
        decode(&p, |f| n += f.len()).unwrap();
        // The first second's page is whole; the damaged one after it is left.
        assert!(n >= 49 * FRAME, "{n}");
    }
}
