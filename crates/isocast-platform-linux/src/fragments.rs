//! What a fragmented movie on disk holds, read back from its boxes: the
//! writer's crash safety (L.S1f).
//!
//! The writer's ffmpeg muxes fragmented MOV/MP4 (`empty_moov`, a `moof`
//! and its `mdat` per fragment), so a file is playable up to its last
//! whole fragment whatever happens to the process. [`Scan`] reads the
//! file's top-level boxes as they arrive: the track list from `moov`,
//! then each fragment's per-track start (`tfdt`) and sample durations
//! (`trun`, else the `tfhd`/`trex` defaults). A fragment counts once its
//! `mdat` is wholly on disk, which is the most a crash can no longer lose.
//! Reading is incremental, so the writer can ask as often as the engine
//! does.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// One track of the movie, from its `moov`.
#[derive(Debug, Clone, Default)]
struct Track {
    id: u32,
    timescale: u32,
    video: bool,
    /// `trex`'s default sample duration.
    default_duration: u32,
    /// Where its whole fragments end, in its timescale, and their samples.
    end: u64,
    samples: u64,
}

/// A fragment whose `moof` has been read and whose `mdat` hasn't yet.
#[derive(Debug, Clone)]
struct Pending {
    /// Per track id: where its runs end, and how many samples they hold.
    ends: Vec<(u32, u64, u64)>,
}

/// How far a fragmented movie's whole fragments reach.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// The next top-level box to read.
    offset: u64,
    /// Where the last whole fragment ends: everything after it is torn.
    pub(crate) whole_end: u64,
    tracks: Vec<Track>,
    /// Whether the `moov` declares fragments (`mvex`).
    fragmented: bool,
    pending: Option<Pending>,
    /// Whole fragments read.
    pub(crate) fragments: u32,
}

impl Scan {
    /// Read whatever whole boxes `path` has gained since the last call.
    /// A file that isn't there yet holds nothing.
    pub(crate) fn advance(&mut self, path: &Path) -> Result<()> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let length = file.metadata()?.len();
        while let Some((kind, size, header)) = box_at(&mut file, self.offset, length)? {
            let body = self.offset + header;
            match &kind {
                b"moov" => {
                    let bytes = read(&mut file, body, size - header)?;
                    self.moov(&bytes)?;
                }
                b"moof" => {
                    let bytes = read(&mut file, body, size - header)?;
                    self.pending = Some(self.moof(&bytes)?);
                }
                b"mdat" => {
                    if let Some(pending) = self.pending.take() {
                        for (id, end, samples) in pending.ends {
                            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) {
                                track.end = track.end.max(end);
                                track.samples += samples;
                            }
                        }
                        self.fragments += 1;
                    }
                    self.whole_end = self.offset + size;
                }
                _ => {
                    // ftyp, free, mfra...: whole, and nothing to count.
                    if self.pending.is_none() {
                        self.whole_end = self.offset + size;
                    }
                }
            }
            self.offset += size;
        }
        Ok(())
    }

    /// How much of the timeline every track's whole fragments cover: the
    /// shortest track's end (a track with no fragment yet covers none).
    pub(crate) fn covered(&self) -> std::time::Duration {
        self.tracks
            .iter()
            .map(|track| seconds(track.end, track.timescale))
            .fold(None::<f64>, |shortest, s| {
                Some(shortest.map_or(s, |m| m.min(s)))
            })
            .map_or(
                std::time::Duration::ZERO,
                std::time::Duration::from_secs_f64,
            )
    }

    /// The pictures in the video track's whole fragments.
    pub(crate) fn frames(&self) -> Option<u64> {
        self.tracks.iter().find(|t| t.video).map(|t| t.samples)
    }

    pub(crate) fn fragmented(&self) -> bool {
        self.fragmented
    }

    /// Track by track: "video 90 pictures (3.000 s); audio 3.000 s".
    pub(crate) fn summary(&self) -> String {
        self.tracks
            .iter()
            .map(|track| {
                let s = seconds(track.end, track.timescale);
                if track.video {
                    format!("video {} pictures ({s:.3} s)", track.samples)
                } else {
                    format!("audio {s:.3} s")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn moov(&mut self, bytes: &[u8]) -> Result<()> {
        for (kind, body) in children(bytes) {
            match &kind {
                b"trak" => {
                    let mut track = Track::default();
                    for (kind, body) in children(body) {
                        match &kind {
                            b"tkhd" => {
                                let version = *body.first().unwrap_or(&0);
                                let at = if version == 1 { 20 } else { 12 };
                                track.id = u32_at(body, at)?;
                            }
                            b"mdia" => {
                                for (kind, body) in children(body) {
                                    match &kind {
                                        b"mdhd" => {
                                            let version = *body.first().unwrap_or(&0);
                                            let at = if version == 1 { 20 } else { 12 };
                                            track.timescale = u32_at(body, at)?;
                                        }
                                        b"hdlr" => track.video = body.get(8..12) == Some(b"vide"),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    self.tracks.push(track);
                }
                b"mvex" => {
                    self.fragmented = true;
                    for (kind, body) in children(body) {
                        if &kind == b"trex" {
                            let id = u32_at(body, 4)?;
                            let duration = u32_at(body, 12)?;
                            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) {
                                track.default_duration = duration;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn moof(&self, bytes: &[u8]) -> Result<Pending> {
        let mut ends = Vec::new();
        for (kind, traf) in children(bytes) {
            if &kind != b"traf" {
                continue;
            }
            let (mut id, mut base, mut default_duration) = (0, 0u64, None);
            let mut durations = 0u64;
            let mut samples = 0u64;
            for (kind, body) in children(traf) {
                match &kind {
                    b"tfhd" => {
                        let flags = u32_at(body, 0)? & 0x00ff_ffff;
                        id = u32_at(body, 4)?;
                        let mut at = 8;
                        if flags & 0x01 != 0 {
                            at += 8; // base-data-offset
                        }
                        if flags & 0x02 != 0 {
                            at += 4; // sample-description-index
                        }
                        if flags & 0x08 != 0 {
                            default_duration = Some(u32_at(body, at)?);
                        }
                    }
                    b"tfdt" => {
                        base = if body.first() == Some(&1) {
                            u64_at(body, 4)?
                        } else {
                            u64::from(u32_at(body, 4)?)
                        };
                    }
                    b"trun" => {
                        let flags = u32_at(body, 0)? & 0x00ff_ffff;
                        let count = u32_at(body, 4)?;
                        let mut at = 8;
                        if flags & 0x001 != 0 {
                            at += 4; // data-offset
                        }
                        if flags & 0x004 != 0 {
                            at += 4; // first-sample-flags
                        }
                        let per_sample = [0x100, 0x200, 0x400, 0x800]
                            .iter()
                            .filter(|bit| flags & *bit != 0)
                            .count()
                            * 4;
                        let fallback = default_duration
                            .or_else(|| {
                                self.tracks
                                    .iter()
                                    .find(|t| t.id == id)
                                    .map(|t| t.default_duration)
                            })
                            .unwrap_or(0);
                        for sample in 0..count as usize {
                            let duration = if flags & 0x100 != 0 {
                                u32_at(body, at + sample * per_sample)?
                            } else {
                                fallback
                            };
                            durations += u64::from(duration);
                        }
                        samples += u64::from(count);
                    }
                    _ => {}
                }
            }
            ends.push((id, base + durations, samples));
        }
        Ok(Pending { ends })
    }
}

fn seconds(ticks: u64, timescale: u32) -> f64 {
    ticks as f64 / f64::from(timescale.max(1))
}

/// The top-level box at `offset`, if it is wholly in the file: its type,
/// size and header length. A box that runs to the file's end (size 0) or
/// past it is not whole yet.
fn box_at(file: &mut File, offset: u64, length: u64) -> Result<Option<([u8; 4], u64, u64)>> {
    if offset + 8 > length {
        return Ok(None);
    }
    let head = read(file, offset, 8)?;
    let mut size = u64::from(u32::from_be_bytes(head[0..4].try_into().unwrap()));
    let kind: [u8; 4] = head[4..8].try_into().unwrap();
    let mut header = 8;
    if size == 1 {
        if offset + 16 > length {
            return Ok(None);
        }
        size = u64::from_be_bytes(read(file, offset + 8, 8)?.try_into().unwrap());
        header = 16;
    }
    if size == 0 || size < header || offset + size > length {
        return Ok(None);
    }
    Ok(Some((kind, size, header)))
}

fn read(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>> {
    if length > 512 << 20 {
        bail!("a {length}-byte box is too large to read");
    }
    let mut bytes = vec![0; length as usize];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The boxes inside a container box's body.
fn children(mut bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while bytes.len() >= 8 {
        let size = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = bytes[4..8].try_into().unwrap();
        if size < 8 || size > bytes.len() {
            break;
        }
        out.push((kind, &bytes[8..size]));
        bytes = &bytes[size..];
    }
    out
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .context("a box is shorter than its fields")
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64> {
    bytes
        .get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .context("a box is shorter than its fields")
}
