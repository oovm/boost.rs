//! Minimal ISO base media file format (MP4) muxer, written from the
//! specification with no third-party container library.
//!
//! Sample data streams into a `mdat` as it is produced — frames never sit in
//! memory until the end of the render — and the `moov` metadata is written
//! after it once the sample table is complete. Every sample is its own
//! chunk, which keeps `stsc` at one entry and makes chunk offsets derivable
//! from the sample sizes alone.
//!
//! The muxer is codec-agnostic: it stamps whatever four-character code and
//! sample-entry children the caller passes (`jpeg` for the portable backend,
//! `avc1` with an `avcC` for a hardware H.264 path, `sowt` for PCM audio).

use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
};

use media_types::Error;

/// Ticks per second on the video track: exact durations for every whole
/// frame rate without fractional ticks.
pub const VIDEO_TIMESCALE: u32 = 1_000_000;

/// One video track: the sample entry identity plus codec-specific children.
pub struct VideoParams {
    /// Sample entry four-character code, e.g. `*b"jpeg"` or `*b"avc1"`.
    pub fourcc: [u8; 4],
    /// Encoded frame width in pixels.
    pub width: u16,
    /// Encoded frame height in pixels.
    pub height: u16,
    /// Serialised child boxes of the sample entry (`avcC`, `pasp`, ...).
    pub entry_children: Vec<u8>,
}

/// One audio track.
pub struct AudioParams {
    /// Sample entry four-character code, e.g. `*b"sowt"` (PCM16 little
    /// endian) or `*b"mp4a"`.
    pub fourcc: [u8; 4],
    /// Sample rate in Hz; it doubles as the track timescale.
    pub sample_rate: u32,
    /// Channel count (1 or 2).
    pub channels: u16,
    /// Bits per sample (16 for the PCM path).
    pub bits: u16,
    /// Serialised child boxes of the sample entry (`esds`, `Qclt`, ...).
    pub entry_children: Vec<u8>,
}

/// Per-track sample bookkeeping collected while data streams out.
struct Track {
    sizes: Vec<u32>,
    durations: Vec<u32>,
    timescale: u32,
}

impl Track {
    fn new(timescale: u32) -> Self {
        Self { sizes: Vec::new(), durations: Vec::new(), timescale }
    }

    fn push(&mut self, size: usize, duration: u32) -> Result<(), Error> {
        if size > u32::MAX as usize {
            return Err(Error::encode("mp4: sample larger than 4 GiB"));
        }
        self.sizes.push(size as u32);
        self.durations.push(duration);
        Ok(())
    }

    fn duration_ticks(&self) -> u64 {
        self.durations.iter().map(|&d| u64::from(d)).sum()
    }
}

/// An open MP4 file being written sample by sample.
pub struct Muxer {
    file: BufWriter<File>,
    /// File offset of the 64-bit size field inside the `mdat` header.
    mdat_size_offset: u64,
    mdat_bytes: u64,
    mdat_payload_start: u64,
    video: Track,
    audio: Option<Track>,
    video_params: VideoParams,
    audio_params: Option<AudioParams>,
}

fn io_error(context: &str, error: std::io::Error) -> Error {
    Error::encode(format!("{context}: {error}"))
}

/// Appends a 32-bit box header plus body.
fn box_out(out: &mut Vec<u8>, fourcc: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
}

/// Appends a full box (version byte and 24-bit flags ahead of the body).
fn full_box(out: &mut Vec<u8>, fourcc: &[u8; 4], version: u8, flags: u32, body: &[u8]) {
    let mut padded = Vec::with_capacity(4 + body.len());
    padded.push(version);
    padded.extend_from_slice(&flags.to_be_bytes()[1..]);
    padded.extend_from_slice(body);
    box_out(out, fourcc, &padded);
}

/// Identity transform matrix (three 16.16 / 2.30 columns, big endian).
const MATRIX: [u8; 36] = {
    let mut m = [0u8; 36];
    m[3] = 1; // a = 0x00010000
    m[11] = 1; // b = 0x00010000
    m[32] = 0x40; // u = 0x40000000
    m
};

impl Muxer {
    /// Creates `path` and writes the `ftyp` and the `mdat` header.
    pub fn open(path: &Path, video: VideoParams, audio: Option<AudioParams>) -> Result<Self, Error> {
        if audio.as_ref().is_some_and(|a| a.channels == 0 || a.channels > 2) {
            return Err(Error::invalid_input("mp4: audio channels must be 1 or 2"));
        }
        if audio.as_ref().is_some_and(|a| a.sample_rate == 0) {
            return Err(Error::invalid_input("mp4: audio sample rate must be positive"));
        }
        let file = File::create(path).map_err(|e| io_error(&format!("create {}", path.display()), e))?;
        let mut file = BufWriter::new(file);

        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(b"isom");
        ftyp.extend_from_slice(&0x0000_0200u32.to_be_bytes());
        ftyp.extend_from_slice(b"isomiso2mp41");
        let mut head = Vec::new();
        box_out(&mut head, b"ftyp", &ftyp);

        // 64-bit `mdat` header: size field = 1, then the real 64-bit size.
        // The fixed 16-byte header lets the payload start be known upfront.
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        let mdat_size_offset = head.len() as u64;
        head.extend_from_slice(&0u64.to_be_bytes());

        let mdat_payload_start = head.len() as u64;
        file.write_all(&head).map_err(|e| io_error("write header", e))?;

        Ok(Self {
            file,
            mdat_size_offset,
            mdat_bytes: 0,
            mdat_payload_start,
            video: Track::new(VIDEO_TIMESCALE),
            audio: audio.as_ref().map(|a| Track::new(a.sample_rate)),
            video_params: video,
            audio_params: audio,
        })
    }

    /// Appends one encoded video sample of `duration` ticks.
    pub fn write_video_sample(&mut self, data: &[u8], duration: u32) -> Result<(), Error> {
        self.video.push(data.len(), duration)?;
        self.file.write_all(data).map_err(|e| io_error("write video sample", e))?;
        self.mdat_bytes += data.len() as u64;
        Ok(())
    }

    /// Appends one encoded audio sample of `duration` ticks.
    pub fn write_audio_sample(&mut self, data: &[u8], duration: u32) -> Result<(), Error> {
        if self.audio.is_none() {
            return Err(Error::invalid_input("mp4: this file was opened without an audio track"));
        }
        self.audio.as_mut().expect("checked").push(data.len(), duration)?;
        self.file.write_all(data).map_err(|e| io_error("write audio sample", e))?;
        self.mdat_bytes += data.len() as u64;
        Ok(())
    }

    /// Patches the `mdat` size and appends the `moov`. Consumes the muxer,
    /// flushing the writer and closing the file.
    pub fn finish(mut self) -> Result<(), Error> {
        let moov = self.build_moov();
        let mdat_size_offset = self.mdat_size_offset;
        let total = 16 + self.mdat_bytes;
        self.file.flush().map_err(|e| io_error("flush mdat", e))?;
        let mut inner = self.file.into_inner().map_err(|e| io_error("close buffer", e.into_error()))?;
        inner.seek(SeekFrom::Start(mdat_size_offset)).map_err(|e| io_error("seek mdat size", e))?;
        inner.write_all(&total.to_be_bytes()).map_err(|e| io_error("patch mdat size", e))?;
        inner.seek(SeekFrom::End(0)).map_err(|e| io_error("seek end", e))?;
        inner.write_all(&moov).map_err(|e| io_error("write moov", e))?;
        inner.flush().map_err(|e| io_error("flush moov", e))
    }

    fn build_moov(&self) -> Vec<u8> {
        let mut moov = Vec::new();
        let video_seconds_ms = |track: &Track| track.duration_ticks() * 1000 / u64::from(track.timescale);
        let video_duration = video_seconds_ms(&self.video);
        let duration = self.audio.as_ref().map_or(video_duration, |a| video_duration.max(video_seconds_ms(a)));

        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0; 8]); // creation
        mvhd.extend_from_slice(&[0; 8]); // modification
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&duration.to_be_bytes());
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend_from_slice(&[0; 10]); // reserved
        mvhd.extend_from_slice(&MATRIX);
        mvhd.extend_from_slice(&[0; 24]); // pre_defined
        mvhd.extend_from_slice(&3u32.to_be_bytes()); // next track id
        full_box(&mut moov, b"mvhd", 1, 0, &mvhd);

        moov.extend_from_slice(&self.build_trak(1, &self.video, &self.video_params.entry_children, true));
        if let (Some(track), Some(params)) = (&self.audio, &self.audio_params) {
            moov.extend_from_slice(&self.build_trak(2, track, &params.entry_children, false));
        }

        let mut out = Vec::new();
        box_out(&mut out, b"moov", &moov);
        out
    }

    fn build_trak(&self, id: u32, track: &Track, entry_children: &[u8], is_video: bool) -> Vec<u8> {
        let mut trak = Vec::new();
        let duration_ticks = track.duration_ticks();
        let duration_ms = duration_ticks * 1000 / u64::from(track.timescale);

        // --- tkhd ---
        let mut tkhd = Vec::new();
        tkhd.extend_from_slice(&[0; 8]); // creation
        tkhd.extend_from_slice(&[0; 8]); // modification
        tkhd.extend_from_slice(&id.to_be_bytes());
        tkhd.extend_from_slice(&[0; 4]); // reserved
        tkhd.extend_from_slice(&duration_ms.to_be_bytes());
        tkhd.extend_from_slice(&[0; 8]); // reserved
        tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
        tkhd.extend_from_slice(&0u16.to_be_bytes()); // alternate group
        let volume: u16 = if is_video { 0 } else { 0x0100 };
        tkhd.extend_from_slice(&volume.to_be_bytes());
        tkhd.extend_from_slice(&[0; 2]); // reserved
        tkhd.extend_from_slice(&MATRIX);
        if is_video {
            tkhd.extend_from_slice(&(u32::from(self.video_params.width) << 16).to_be_bytes());
            tkhd.extend_from_slice(&(u32::from(self.video_params.height) << 16).to_be_bytes());
        } else {
            tkhd.extend_from_slice(&[0; 8]); // width/height zero for audio
        }
        // flags: enabled | in movie | (preview for video)
        full_box(&mut trak, b"tkhd", 1, if is_video { 7 } else { 3 }, &tkhd);

        let mut mdia = Vec::new();

        // --- mdhd ---
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0; 8]);
        mdhd.extend_from_slice(&[0; 8]);
        mdhd.extend_from_slice(&track.timescale.to_be_bytes());
        mdhd.extend_from_slice(&duration_ticks.to_be_bytes());
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // "und"
        mdhd.extend_from_slice(&0u16.to_be_bytes());
        full_box(&mut mdia, b"mdhd", 1, 0, &mdhd);

        // --- hdlr ---
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&0u32.to_be_bytes());
        hdlr.extend_from_slice(if is_video { b"vide" } else { b"soun" });
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.push(0); // empty name
        full_box(&mut mdia, b"hdlr", 0, 0, &hdlr);

        let mut minf = Vec::new();
        if is_video {
            full_box(&mut minf, b"vmhd", 1, 1, &[0; 8]); // graphicsmode 0, opcolor 0
        } else {
            full_box(&mut minf, b"smhd", 0, 0, &[0; 4]); // balance 0
        }

        // --- dinf > dref > url (self-contained) ---
        let mut dref_body = Vec::new();
        dref_body.extend_from_slice(&1u32.to_be_bytes());
        let mut url = Vec::new();
        full_box(&mut url, b"url ", 0, 1, &[]);
        dref_body.extend_from_slice(&url);
        let mut dinf = Vec::new();
        full_box(&mut dinf, b"dref", 0, 0, &dref_body);
        box_out(&mut minf, b"dinf", &dinf);

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&self.build_stsd(is_video, entry_children));
        stbl.extend_from_slice(&build_stts(&track.durations));

        // One sample per chunk: a single stsc entry covers the whole track.
        let mut stsc = Vec::new();
        stsc.extend_from_slice(&1u32.to_be_bytes()); // entry count
        stsc.extend_from_slice(&1u32.to_be_bytes()); // first chunk
        stsc.extend_from_slice(&1u32.to_be_bytes()); // samples per chunk
        stsc.extend_from_slice(&1u32.to_be_bytes()); // sample description index
        full_box(&mut stbl, b"stsc", 0, 0, &stsc);

        // --- stsz ---
        let uniform = track.sizes.first().is_some_and(|&first| track.sizes.iter().all(|&s| s == first));
        let shared_size = if uniform { track.sizes.first().copied().unwrap_or(0) } else { 0 };
        let mut stsz = Vec::new();
        stsz.extend_from_slice(&shared_size.to_be_bytes());
        stsz.extend_from_slice(&(track.sizes.len() as u32).to_be_bytes());
        if !uniform {
            for &size in &track.sizes {
                stsz.extend_from_slice(&size.to_be_bytes());
            }
        }
        full_box(&mut stbl, b"stsz", 0, 0, &stsz);

        // --- stco: every sample is its own chunk, offsets accumulate ---
        let mut stco = Vec::new();
        stco.extend_from_slice(&(track.sizes.len() as u32).to_be_bytes());
        let mut offset = self.mdat_payload_start;
        for &size in &track.sizes {
            if offset > u32::MAX as u64 {
                break; // a >4 GiB file needs `co64`; out of scope for the fallback
            }
            stco.extend_from_slice(&(offset as u32).to_be_bytes());
            offset += u64::from(size);
        }
        full_box(&mut stbl, b"stco", 0, 0, &stco);

        box_out(&mut minf, b"stbl", &stbl);
        box_out(&mut mdia, b"minf", &minf);
        box_out(&mut trak, b"mdia", &mdia);

        let mut out = Vec::new();
        box_out(&mut out, b"trak", &trak);
        out
    }

    fn build_stsd(&self, is_video: bool, entry_children: &[u8]) -> Vec<u8> {
        let mut entry = Vec::new();
        let fourcc;
        if is_video {
            let params = &self.video_params;
            fourcc = params.fourcc;
            entry.extend_from_slice(&[0; 6]); // reserved
            entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
            entry.extend_from_slice(&[0; 4]); // pre_defined + reserved
            entry.extend_from_slice(&[0; 12]); // pre_defined[3]
            entry.extend_from_slice(&params.width.to_be_bytes());
            entry.extend_from_slice(&params.height.to_be_bytes());
            entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi horizontal
            entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi vertical
            entry.extend_from_slice(&0u32.to_be_bytes()); // reserved
            entry.extend_from_slice(&1u32.to_be_bytes()); // frame count
            entry.extend_from_slice(&[0; 32]); // compressor name
            entry.extend_from_slice(&24u16.to_be_bytes()); // depth
            entry.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
        } else {
            let params = self.audio_params.as_ref().expect("audio track implies params");
            fourcc = params.fourcc;
            entry.extend_from_slice(&[0; 6]); // reserved
            entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
            entry.extend_from_slice(&[0; 4]); // version + revision
            entry.extend_from_slice(&0u32.to_be_bytes()); // reserved
            entry.extend_from_slice(&params.channels.to_be_bytes());
            entry.extend_from_slice(&params.bits.to_be_bytes());
            entry.extend_from_slice(&[0; 4]); // pre_defined + reserved
            entry.extend_from_slice(&(params.sample_rate << 16).to_be_bytes()); // 16.16 rate
        }
        entry.extend_from_slice(entry_children);

        let mut body = Vec::new();
        box_out(&mut body, &fourcc, &entry);
        let mut stsd = Vec::new();
        stsd.extend_from_slice(&1u32.to_be_bytes()); // entry count
        stsd.extend_from_slice(&body);
        let mut out = Vec::new();
        full_box(&mut out, b"stsd", 0, 0, &stsd);
        out
    }
}

/// Run-length encodes the sample durations into an `stts` box.
fn build_stts(durations: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut runs: Vec<(u32, u32)> = Vec::new(); // (count, delta)
    for &d in durations {
        match runs.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => runs.push((1, d)),
        }
    }
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&delta.to_be_bytes());
    }
    let mut out = Vec::new();
    full_box(&mut out, b"stts", 0, 0, &body);
    out
}

#[cfg(test)]
mod tests {
    use super::{AudioParams, Muxer, VIDEO_TIMESCALE, VideoParams};

    /// Splits one box body into its child boxes.
    fn children(body: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= body.len() {
            let size = u32::from_be_bytes(body[at..at + 4].try_into().unwrap()) as u64;
            let fourcc: [u8; 4] = body[at + 4..at + 8].try_into().unwrap();
            let mut payload = at + 8;
            let mut total = size;
            if size == 1 {
                total = u64::from_be_bytes(body[at + 8..at + 16].try_into().unwrap());
                payload = at + 16;
            } else if size == 0 {
                total = (body.len() - at) as u64;
            }
            out.push((fourcc, &body[payload..at + total as usize]));
            at += total as usize;
        }
        out
    }

    /// Walks a chain of container boxes (no version header) into a body.
    fn find<'a>(src: &'a [u8], path: &[[u8; 4]]) -> Option<&'a [u8]> {
        let (head, rest) = path.split_first()?;
        for (fourcc, body) in children(src) {
            if &fourcc == head {
                if rest.is_empty() {
                    return Some(body);
                }
                return find(body, rest);
            }
        }
        None
    }

    fn sample_file(path: &std::path::Path) {
        let mut muxer = Muxer::open(
            path,
            VideoParams { fourcc: *b"jpeg", width: 32, height: 16, entry_children: Vec::new() },
            Some(AudioParams { fourcc: *b"sowt", sample_rate: 48_000, channels: 2, bits: 16, entry_children: Vec::new() }),
        )
        .expect("open muxer");
        let frames: [&[u8]; 3] = [b"frame-one!!", b"frame-two!!", b"frame-three!!!"];
        for frame in frames {
            muxer.write_video_sample(frame, VIDEO_TIMESCALE / 30).expect("video sample");
        }
        muxer.write_audio_sample(b"aaaa", 480).expect("audio sample");
        muxer.write_audio_sample(b"bb", 240).expect("audio sample");
        muxer.finish().expect("finish");
    }

    #[test]
    fn lays_out_ftyp_mdat_moov_with_seekable_samples() {
        let path = std::env::temp_dir().join("media-codec-mp4-layout.mp4");
        sample_file(&path);
        let bytes = std::fs::read(&path).expect("read back");

        let tops = children(&bytes);
        let kinds: Vec<_> = tops.iter().map(|(f, _)| *f).collect();
        assert_eq!(kinds, [*b"ftyp", *b"mdat", *b"moov"]);

        let mdat = tops[1].1;
        assert_eq!(mdat, b"frame-one!!frame-two!!frame-three!!!aaaabb");

        let moov = tops[2].1;
        let traks: Vec<_> = children(moov).into_iter().filter(|(f, _)| *f == *b"trak").collect();
        assert_eq!(traks.len(), 2);

        // Video sample table.
        let stsd = find(moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"]).expect("stsd");
        let entries = children(&stsd[8..]); // skip full-box header + entry count
        assert_eq!(entries[0].0, *b"jpeg");
        let stsz = find(moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsz"]).expect("stsz");
        // Varying sizes: sample_size 0, count 3, then the table.
        assert_eq!(&stsz[4..8], &[0, 0, 0, 0]);
        assert_eq!(&stsz[8..12], &[0, 0, 0, 3]);
        let stco = find(moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stco"]).expect("stco");
        let count = u32::from_be_bytes(stco[4..8].try_into().unwrap());
        assert_eq!(count, 3);
        let first_offset = u32::from_be_bytes(stco[8..12].try_into().unwrap()) as usize;
        assert_eq!(&bytes[first_offset..first_offset + 11], b"frame-one!!");

        // Audio sample entry carries rate and channel layout.
        let audio_stsd = find(traks[1].1, &[*b"mdia", *b"minf", *b"stbl", *b"stsd"]).expect("audio stsd");
        let audio_entries = children(&audio_stsd[8..]);
        assert_eq!(audio_entries[0].0, *b"sowt");
        let entry = audio_entries[0].1;
        assert_eq!(&entry[16..18], &[0, 2]); // channels
        assert_eq!(&entry[18..20], &[0, 16]); // bits
        assert_eq!(u32::from_be_bytes(entry[24..28].try_into().unwrap()), 48_000 << 16);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn uniform_sizes_collapse_the_size_table() {
        let path = std::env::temp_dir().join("media-codec-mp4-uniform.mp4");
        let mut muxer =
            Muxer::open(&path, VideoParams { fourcc: *b"jpeg", width: 8, height: 8, entry_children: Vec::new() }, None).expect("open");
        for _ in 0..4 {
            muxer.write_video_sample(b"same", 1).expect("sample");
        }
        muxer.finish().expect("finish");
        let bytes = std::fs::read(&path).expect("read");
        let moov = children(&bytes).into_iter().find(|(f, _)| *f == *b"moov").expect("moov").1;
        let stsz = find(moov, &[*b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsz"]).expect("stsz");
        assert_eq!(&stsz[4..8], &[0, 0, 0, 4]); // uniform size
        assert_eq!(&stsz[8..12], &[0, 0, 0, 4]); // count
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn audio_without_a_track_is_rejected() {
        let path = std::env::temp_dir().join("media-codec-mp4-silent.mp4");
        let mut muxer =
            Muxer::open(&path, VideoParams { fourcc: *b"jpeg", width: 8, height: 8, entry_children: Vec::new() }, None).expect("open");
        let error = muxer.write_audio_sample(b"x", 1).expect_err("must fail");
        assert_eq!(error.code(), "invalid_input");
        muxer.finish().expect("finish");
        std::fs::remove_file(&path).ok();
    }
}
