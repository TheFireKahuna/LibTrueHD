//! The decoder behind a handle: framing, parsing, decoding, PCM conversion, timing and
//! recovery. Everything here is safe Rust; `lib.rs` wraps it for C.

use crate::framer::{AuInfo, Framer, Next};
use crate::objects::ObjectState;
use crate::stream::{self, StreamDesc};
use crate::types::*;
use std::any::Any;
use std::collections::VecDeque;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use truehd::process::MAX_PRESENTATIONS;
use truehd::process::decode::{DecodedAccessUnit, Decoder};
use truehd::process::extract::Frame;
use truehd::process::parse::Parser;
use truehd::structs::oamd::ObjectAudioMetadataPayload;

/// Input the caller may push ahead of pulling before push() refuses more.
const MAX_PENDING_INPUT: usize = 8 << 20;
/// Pushes whose bytes are still unconsumed, tracked for pts attribution.
const MAX_PENDING_CHUNKS: usize = 1 << 16;
/// Evolution payload id of object audio metadata.
const OAMD_PAYLOAD_ID: u32 = 11;
const PCM_SCALE: f32 = 1.0 / 8_388_608.0;

struct Chunk {
    start: u64,
    pts: i64,
    serial: u64,
}

/// Channel roles as blocks report them; `layout_serial` changes exactly when these do.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Roles {
    sample_rate: u32,
    presentation: u32,
    channels: u32,
    speakers: [u8; MAX_CHANNELS],
}

pub struct Engine {
    selection: i32,
    /// Also decode the highest channel-based presentation alongside an object presentation.
    bed_companion: bool,
    bed_resolved: Option<usize>,
    framer: Framer,
    parser: Box<Parser>,
    decoder: Box<Decoder>,
    /// Decoding restarts at the next major sync.
    waiting_for_sync: bool,
    /// Major sync info of the configuration the parser and decoder are set up for.
    sync_signature: Vec<u8>,
    signature_scratch: Vec<u8>,
    desc: Option<StreamDesc>,
    resolved: usize,
    required: [bool; MAX_PRESENTATIONS],
    oamd_seen: bool,
    /// The stream's presentation clock (see the header's SOURCE POSITION): the ordinal of the
    /// access unit whose restart header was read last, and its output_timing.
    output_clock: Option<(u64, u16)>,
    /// Ordinal of the first access unit the parser took since it was reset, which is its
    /// access unit 0, and the 32-bit position hires_output_timing gives that access unit.
    parse_origin: Option<u64>,
    hires_origin: Option<(u64, u32)>,

    chunks: VecDeque<Chunk>,
    chunk_serial: u64,
    pushed: u64,
    au_chunk: Option<u64>,
    au_in_chunk: u32,

    sample_position: u64,
    pending_flags: u32,
    roles: Option<Roles>,
    layout_serial: u32,

    planes: Box<[[f32; MAX_BLOCK_FRAMES]; MAX_CHANNELS]>,
    plane_ptrs: [*const f32; MAX_CHANNELS],
    speakers: [u8; MAX_CHANNELS],
    bed_planes: Box<[[f32; MAX_BLOCK_FRAMES]; MAX_CHANNELS]>,
    bed_plane_ptrs: [*const f32; MAX_CHANNELS],
    bed_speakers: [u8; MAX_CHANNELS],
    objects: ObjectState,
    oamd_bytes: Vec<(Option<u64>, Vec<u8>)>,

    stats: Stats,
    last_error: CString,
    pub poisoned: bool,
}

impl Engine {
    pub fn new(selection: i32) -> Box<Self> {
        let mut engine = Box::new(Self {
            selection,
            bed_companion: false,
            bed_resolved: None,
            framer: Framer::default(),
            parser: Box::new(Parser::default()),
            decoder: Box::new(Decoder::default()),
            waiting_for_sync: true,
            sync_signature: Vec::with_capacity(64),
            signature_scratch: Vec::with_capacity(64),
            desc: None,
            resolved: 0,
            required: [false; MAX_PRESENTATIONS],
            oamd_seen: false,
            output_clock: None,
            parse_origin: None,
            hires_origin: None,
            chunks: VecDeque::with_capacity(64),
            chunk_serial: 0,
            pushed: 0,
            au_chunk: None,
            au_in_chunk: 0,
            sample_position: 0,
            pending_flags: BLOCK_DISCONTINUITY,
            roles: None,
            layout_serial: 0,
            planes: Box::new([[0.0; MAX_BLOCK_FRAMES]; MAX_CHANNELS]),
            plane_ptrs: [std::ptr::null(); MAX_CHANNELS],
            speakers: [SPEAKER_UNKNOWN; MAX_CHANNELS],
            bed_planes: Box::new([[0.0; MAX_BLOCK_FRAMES]; MAX_CHANNELS]),
            bed_plane_ptrs: [std::ptr::null(); MAX_CHANNELS],
            bed_speakers: [SPEAKER_UNKNOWN; MAX_CHANNELS],
            objects: ObjectState::default(),
            oamd_bytes: Vec::with_capacity(4),
            stats: Stats::default(),
            last_error: CString::default(),
            poisoned: false,
        });
        engine.parser.set_check_fifo(false);
        for (ptr, plane) in engine.plane_ptrs.iter_mut().zip(engine.planes.iter()) {
            *ptr = plane.as_ptr();
        }
        for (ptr, plane) in engine.bed_plane_ptrs.iter_mut().zip(engine.bed_planes.iter()) {
            *ptr = plane.as_ptr();
        }
        engine
    }

    /// Seek: drop input and decode state. Stream description, the layout serial and the
    /// substream-count hint survive, so a seek within one stream keeps its layout identity.
    pub fn reset(&mut self) {
        self.framer.reset();
        self.restart_decoding();
        self.chunks.clear();
        self.pushed = 0;
        self.au_chunk = None;
        self.au_in_chunk = 0;
        self.sample_position = 0;
        self.oamd_seen = false;
        self.stats = Stats::default();
        self.last_error = CString::default();
        self.poisoned = false;
    }

    pub fn selection(&self) -> i32 {
        self.selection
    }

    pub fn set_presentation(&mut self, selection: i32) {
        self.selection = selection;
        self.sync_signature.clear(); // re-resolve at the next major sync
        self.restart_decoding();
    }

    /// Bed companion: with an object presentation selected, every block also carries the
    /// stream's highest channel-based presentation, decoded from the same access unit.
    /// Applies from the next major sync, like a presentation switch.
    pub fn set_bed_companion(&mut self, on: bool) {
        if self.bed_companion == on {
            return;
        }
        self.bed_companion = on;
        self.sync_signature.clear();
        self.restart_decoding();
    }

    /// Discard matrix interpolation values from now on; see the header.
    pub fn set_ignore_interpolation(&mut self, ignore: bool) {
        self.parser.set_ignore_interpolation(ignore);
    }

    fn restart_decoding(&mut self) {
        self.parser.reset_for_next_major_sync();
        self.output_clock = None;
        self.parse_origin = None;
        self.hires_origin = None;
        self.decoder.reset_for_next_major_sync();
        self.waiting_for_sync = true;
        self.objects.clear();
        self.pending_flags |= BLOCK_DISCONTINUITY;
    }

    fn fail(&mut self, message: String) {
        self.stats.decode_errors += 1;
        self.set_error(message);
        self.restart_decoding();
    }

    fn set_error(&mut self, message: String) {
        self.last_error = CString::new(message.replace('\0', " ")).unwrap_or_default();
    }

    pub fn last_error(&self) -> &CString {
        &self.last_error
    }

    pub fn push(&mut self, data: &[u8], pts: i64) -> Status {
        if data.is_empty() {
            return OK;
        }
        if self.framer.pending() + data.len() > MAX_PENDING_INPUT {
            return ERR_BUFFER_FULL;
        }
        // Past this many pushes waiting (tiny pushes while no major sync has turned up), new
        // bytes join the previous push's record rather than being refused. The record keeps
        // its pts only if the new push has the same one; otherwise the access units starting
        // anywhere in it report no pts rather than a wrong one.
        if self.chunks.len() < MAX_PENDING_CHUNKS {
            self.chunks.push_back(Chunk {
                start: self.pushed,
                pts,
                serial: self.chunk_serial,
            });
            self.chunk_serial += 1;
        } else if let Some(last) = self.chunks.back_mut()
            && last.pts != pts
        {
            last.pts = NO_PTS;
        }
        self.pushed += data.len() as u64;
        self.stats.bytes_pushed += data.len() as u64;
        self.framer.push(data);
        OK
    }

    /// The pts of the push holding `offset`, and how many access units began earlier in it.
    fn attribute(&mut self, offset: u64) -> (i64, u32) {
        while self.chunks.len() >= 2 && self.chunks[1].start <= offset {
            self.chunks.pop_front();
        }
        let Some(chunk) = self.chunks.front().filter(|c| c.start <= offset) else {
            return (NO_PTS, 0);
        };
        if self.au_chunk == Some(chunk.serial) {
            self.au_in_chunk += 1;
        } else {
            self.au_chunk = Some(chunk.serial);
            self.au_in_chunk = 0;
        }
        (chunk.pts, self.au_in_chunk)
    }

    pub fn pull(&mut self, out: &mut Block) -> Status {
        loop {
            let errors_before = self.framer.sync_errors;
            let (info, frame) = match self.framer.next() {
                Next::NeedData => {
                    self.sync_framer_stats(errors_before);
                    return NEED_MORE_DATA;
                }
                Next::Skipped(info) => (info, None),
                Next::Au(info, bytes) => {
                    // Only access units that will be parsed need their own copy.
                    let frame = (!self.waiting_for_sync || info.major_sync).then(|| Frame {
                        timestamp: None,
                        data: Arc::from(bytes),
                        index: info.ordinal,
                        offset: info.offset,
                    });
                    (info, frame)
                }
            };
            self.sync_framer_stats(errors_before);
            let (pts, pts_au_index) = self.attribute(info.offset);

            let Some(frame) = frame else {
                self.stats.access_units_skipped += 1;
                continue;
            };

            if info.major_sync && !self.configure(&frame) {
                continue;
            }

            match self.decode(&frame) {
                Err(message) => {
                    self.fail(message);
                    continue;
                }
                Ok(None) => continue,
                Ok(Some((decoded, index, bed))) => {
                    self.waiting_for_sync = false;
                    self.stats.access_units_decoded += 1;
                    if decoded.is_duplicate {
                        // Encoder duplicate at a splice: its audio (and metadata) repeat the
                        // previous access unit. Dropped, as truehdd does.
                        self.stats.duplicate_access_units += 1;
                        continue;
                    }
                    self.emit(&decoded, index, bed.as_ref(), &info, (pts, pts_au_index), out);
                    return OK;
                }
            }
        }
    }

    fn sync_framer_stats(&mut self, errors_before: u64) {
        if self.framer.sync_errors != errors_before {
            self.stats.sync_errors += self.framer.sync_errors - errors_before;
            self.set_error(
                "bitstream damage: access units failed their checks; resynchronised at a major sync"
                    .into(),
            );
            if !self.waiting_for_sync {
                self.restart_decoding();
            }
        }
    }

    /// At a major sync: learn the stream on a configuration change and set the parser and
    /// decoder up for the presentation to decode. Returns false to skip this access unit.
    fn configure(&mut self, frame: &Frame) -> bool {
        if !stream::major_sync_signature(frame.as_ref(), &mut self.signature_scratch) {
            return true;
        }
        if self.sync_signature == self.signature_scratch && self.desc.is_some() {
            return true;
        }

        let probed = catch_unwind(AssertUnwindSafe(|| stream::probe(frame)))
            .unwrap_or_else(|p| Err(format!("probe panicked: {}", panic_message(&p))));
        let desc = match probed {
            Ok(desc) => desc,
            Err(message) => {
                // Still decodable with the selector alone; the probe retries next major sync.
                self.set_error(message);
                self.apply_required(fallback_resolution(self.selection), None);
                return true;
            }
        };
        let resolved = stream::resolve(self.selection, &desc.map);
        let bed = stream::resolve(PRESENTATION_HIGHEST_CHANNEL_BASED, &desc.map);
        let configured_before = !self.sync_signature.is_empty();
        std::mem::swap(&mut self.sync_signature, &mut self.signature_scratch);
        // Another stream configuration: what its payloads' interval said no longer holds.
        self.objects.forget_timing();
        self.desc = Some(desc);
        if configured_before && !self.waiting_for_sync {
            // A configuration change mid-stream: start the new one cleanly at this access unit.
            self.restart_decoding();
        }
        let bed = (self.bed_companion && resolved == 3 && bed != resolved).then_some(bed);
        self.apply_required(resolved, bed);
        true
    }

    fn apply_required(&mut self, resolved: usize, bed: Option<usize>) {
        self.resolved = resolved;
        self.bed_resolved = bed;
        self.required = [false; MAX_PRESENTATIONS];
        self.required[resolved] = true;
        if let Some(bed) = bed {
            self.required[bed] = true;
        }
        self.parser.set_required_presentations(&self.required);
    }

    /// Parses and decodes one access unit. `Ok(None)` for an access unit skipped while waiting
    /// for a major sync.
    fn decode(
        &mut self,
        frame: &Frame,
    ) -> Result<Option<(DecodedAccessUnit, usize, Option<DecodedAccessUnit>)>, String> {
        if self.waiting_for_sync && !frame.is_major_sync() {
            self.stats.access_units_skipped += 1;
            return Ok(None);
        }

        let parser = &mut self.parser;
        let mut au = catch_unwind(AssertUnwindSafe(|| parser.parse(frame)))
            .map_err(|p| format!("parser panicked: {}", panic_message(&p)))?
            .map_err(|e| format!("parse error: {e:#}"))?;
        // Branch records only grow; nothing here reads them.
        drop(self.parser.take_branches());

        let origin = *self.parse_origin.get_or_insert(frame.index);
        if let Some(output_timing) = au
            .substream_segment
            .iter()
            .flat_map(|segment| segment.block.iter())
            .find_map(|block| block.restart_header.as_ref().map(|rh| rh.output_timing))
        {
            self.output_clock = Some((frame.index, output_timing));
        }
        if self.hires_origin.is_none() {
            // The parser states the position of its access unit 0.
            self.hires_origin = self.parser.hires_output_timing().map(|start| (origin, start as u32));
        }

        // Object metadata is parsed here rather than by the decoder, which would fail the whole
        // access unit (audio included) on a payload it cannot read.
        self.oamd_bytes.clear();
        if let Some(evo) = au.extra_data.as_mut().and_then(|x| x.evo_frame.as_mut()) {
            for payload in evo.evo_payloads.iter_mut() {
                if payload.evo_payload_id == OAMD_PAYLOAD_ID {
                    let offset = payload.evo_payload_config.smploffst.map(u64::from);
                    self.oamd_bytes
                        .push((offset, std::mem::take(&mut payload.evo_payload_byte)));
                    payload.evo_payload_id = 0;
                }
            }
        }

        let decoder = &mut self.decoder;
        let required = self.required;
        let mut decoded = catch_unwind(AssertUnwindSafe(|| {
            decoder.decode_presentations(&au, &required)
        }))
        .map_err(|p| format!("decoder panicked: {}", panic_message(&p)))?
        .map_err(|e| format!("decode error: {e:#}"))?;

        let bed = self.bed_resolved.and_then(|i| decoded[i].take());
        let (main, index) = decoded
            .iter_mut()
            .enumerate()
            .find_map(|(i, slot)| slot.take().map(|d| (d, i)))
            .ok_or_else(|| "decoder produced no presentation".to_string())?;
        Ok(Some((main, index, bed)))
    }

    fn emit(
        &mut self,
        decoded: &DecodedAccessUnit,
        presentation: usize,
        bed: Option<&DecodedAccessUnit>,
        info: &AuInfo,
        (pts, pts_au_index): (i64, u32),
        out: &mut Block,
    ) {
        let frames = decoded.sample_length.min(MAX_BLOCK_FRAMES);
        let channels = decoded.channel_count.min(MAX_CHANNELS);

        for (c, plane) in self.planes.iter_mut().enumerate().take(channels) {
            for (dst, row) in plane.iter_mut().zip(decoded.pcm_data.iter()).take(frames) {
                *dst = row[c] as f32 * PCM_SCALE;
            }
        }

        let bed_channels = match bed {
            Some(bed) if bed.sample_length.min(MAX_BLOCK_FRAMES) == frames => {
                let n = bed.channel_count.min(MAX_CHANNELS);
                for (c, plane) in self.bed_planes.iter_mut().enumerate().take(n) {
                    for (dst, row) in plane.iter_mut().zip(bed.pcm_data.iter()).take(frames) {
                        *dst = row[c] as f32 * PCM_SCALE;
                    }
                }
                self.bed_speakers = stream::roles(&bed.channel_labels, n, false);
                n
            }
            _ => 0,
        };

        // Channel roles from the major sync; for an object presentation the bed labels come
        // first and the remaining channels are objects.
        let objects = presentation == 3 && self.desc.as_ref().is_some_and(|d| d.has_objects);
        let roles_from_sync = stream::roles(&decoded.channel_labels, channels, objects);

        // Metadata for this block, after its payloads are scheduled.
        if presentation == 3 {
            let payloads = std::mem::take(&mut self.oamd_bytes);
            for (offset, bytes) in &payloads {
                let (offset, bytes) = (*offset, bytes.as_slice());
                let parsed =
                    catch_unwind(AssertUnwindSafe(|| ObjectAudioMetadataPayload::read(bytes)));
                let mismatches = self.objects.offset_mismatches;
                let result = match parsed {
                    Ok(Ok(payload)) => {
                        self.objects
                            .ingest(&payload, self.sample_position, offset, channels, decoded.sampling_frequency)
                    }
                    Ok(Err(e)) => Err(format!("OAMD parse error: {e:#}")),
                    Err(p) => Err(format!("OAMD parser panicked: {}", panic_message(&p))),
                };
                match result {
                    Ok(()) => {
                        self.stats.oamd_payloads += 1;
                        self.oamd_seen = true;
                        if self.objects.offset_mismatches != mismatches {
                            self.stats.oamd_offset_mismatches += 1;
                            self.set_error(format!(
                                "OAMD payload's EMDF smploffst ({}) differs from its sample_offset; timed by sample_offset",
                                offset.unwrap_or(0)
                            ));
                        }
                    }
                    Err(message) => {
                        self.stats.oamd_errors += 1;
                        self.set_error(message);
                    }
                }
            }
            // Keep the allocation; the payload buffers themselves came from the parser.
            self.oamd_bytes = payloads;
            self.oamd_bytes.clear();
        }
        let block_metadata = self.objects.begin_block(self.sample_position, frames as u32);

        // For an object presentation the OAMD layout in force decides the roles; before the
        // first payload since reset the major sync is all there is. They agree on conforming
        // streams, so the roles (and layout_serial) do not move when metadata arrives.
        let mut speakers = match self.objects.layout() {
            Some(layout) if objects && layout.element_count == channels => layout.speakers,
            _ => roles_from_sync,
        };
        for s in speakers.iter_mut().skip(channels) {
            *s = SPEAKER_UNKNOWN;
        }
        self.speakers = speakers;

        let roles = Roles {
            sample_rate: decoded.sampling_frequency,
            presentation: presentation as u32,
            channels: channels as u32,
            speakers,
        };
        let mut flags = std::mem::take(&mut self.pending_flags);
        if self.roles != Some(roles) {
            flags |= BLOCK_LAYOUT_CHANGED;
            self.layout_serial = self.layout_serial.wrapping_add(1);
            self.roles = Some(roles);
        }
        if !block_metadata.metadata.is_null() {
            flags |= BLOCK_HAS_METADATA;
            if block_metadata.update_count > 0 {
                flags |= BLOCK_METADATA_UPDATED;
                self.stats.metadata_updates += block_metadata.update_count as u64;
            }
        }
        if self.framer.estimated() {
            flags |= BLOCK_INPUT_OFFSET_ESTIMATED;
        }

        let access_unit_frames = self
            .desc
            .as_ref()
            .map_or(decoded.sample_length as u32, |d| d.access_unit_frames);
        let frames_since = |ordinal: u64| ((info.ordinal - ordinal) as u32).wrapping_mul(access_unit_frames);
        let output_timing = self
            .output_clock
            .map_or(0, |(ordinal, timing)| timing.wrapping_add(frames_since(ordinal) as u16));
        // hires_output_timing picks the 2^16-frame window; output_timing is the exact frame in it.
        let output_position = self.hires_origin.map(|(ordinal, start)| {
            let estimate = start.wrapping_add(frames_since(ordinal));
            estimate.wrapping_sub((estimate as u16).wrapping_sub(output_timing) as i16 as i32 as u32)
        });
        if output_position.is_some() {
            flags |= BLOCK_OUTPUT_POSITION;
        }
        *out = Block {
            sample_rate: decoded.sampling_frequency,
            frame_count: frames as u32,
            channel_count: channels as u32,
            presentation: presentation as u32,
            flags,
            access_unit_frames,
            pts,
            pts_au_index,
            layout_serial: self.layout_serial,
            sample_position: self.sample_position,
            input_access_unit: info.ordinal,
            input_frame_offset: info.ordinal * access_unit_frames as u64,
            channels: self.plane_ptrs.as_ptr(),
            speakers: self.speakers.as_ptr(),
            metadata: block_metadata.metadata,
            bed_channel_count: bed_channels as u32,
            bed_channels: if bed_channels > 0 {
                self.bed_plane_ptrs.as_ptr()
            } else {
                std::ptr::null()
            },
            bed_speakers: if bed_channels > 0 {
                self.bed_speakers.as_ptr()
            } else {
                std::ptr::null()
            },
            input_timing: info.input_timing as u32,
            output_timing: output_timing as u32,
            output_position: output_position.unwrap_or(0),
        };

        self.sample_position += frames as u64;
        self.stats.blocks_output += 1;
        self.stats.frames_output += frames as u64;
    }

    pub fn stream_info(&self) -> Option<StreamInfo> {
        let desc = self.desc.as_ref()?;
        Some(StreamInfo {
            format: desc.format,
            sample_rate: desc.sample_rate,
            access_unit_frames: desc.access_unit_frames,
            substream_count: desc.substreams,
            selected_presentation: self.resolved as u32,
            immersive: desc.immersive as u32,
            has_objects: desc.has_objects as u32,
            oamd_seen: self.oamd_seen as u32,
            layout_serial: self.layout_serial,
            presentations: desc.presentations,
        })
    }

    pub fn trims(&self) -> [Trim; TRIM_CONFIGS] {
        self.objects.trims()
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.stats;
        stats.bytes_pending = self.framer.pending() as u64;
        stats
    }
}

/// Presentation to require when the stream could not be probed: the selector as upstream
/// would read it, with its own fallback to the highest available presentation.
fn fallback_resolution(selection: i32) -> usize {
    match selection {
        PRESENTATION_HIGHEST => 3,
        PRESENTATION_HIGHEST_CHANNEL_BASED => 2,
        n => n.clamp(0, 3) as usize,
    }
}

pub fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
