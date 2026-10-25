//! Object audio metadata: turning OAMD payloads into timed element updates and handing each
//! update out with the block that contains its start.

use crate::stream::{oamd_bed_position, speaker_of_oamd_bed};
use crate::types::*;
use std::collections::VecDeque;
use truehd::structs::oamd::{GAIN_MINUS_INFINITY, ObjectAudioMetadataPayload, TrimElement};

/// Updates waiting for their block. Payloads arrive every ~38 access units and state at most
/// eight updates each, none due more than a few thousand samples ahead; anything beyond this
/// is a malformed stream and is dropped rather than buffered without bound.
const MAX_PENDING: usize = 64;
/// TS 103 420 5.3: a payload describes one frame of this many samples ("the decoder shall add the
/// value 1 536 to frame_offset" per frame), which is what defines the unit its timing counts in.
const OAMD_FRAME: u64 = 1536;
/// Every pending update can fall in one block (a stream may send payloads more often than one per
/// 1536-sample frame), and each gets its own entry, so a block holds as many as can be pending.
const MAX_UPDATES_PER_BLOCK: usize = MAX_PENDING;

pub type Elements = [Element; MAX_CHANNELS];

/// Channel roles as the OAMD states them: element kinds, speakers and counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    pub element_count: usize,
    pub bed_count: usize,
    pub isf_count: usize,
    pub dynamic_count: usize,
    pub speakers: [u8; MAX_CHANNELS],
}

struct Pending {
    due: u64,
    ramp: u32,
    layout: Layout,
    elements: Elements,
    /// The trim element in force for this update: its payload's, or the last one's.
    warp_mode: i32,
    trims: [Trim; TRIM_CONFIGS],
}

pub struct ObjectState {
    have_state: bool,
    layout: Option<Layout>,
    /// Of the latest update delivered (what blocks report), and of the latest payload read (what the
    /// updates still pending will carry).
    warp_mode: i32,
    trims: [Trim; TRIM_CONFIGS],
    signalled_warp_mode: i32,
    signalled_trims: [Trim; TRIM_CONFIGS],
    /// Per element: trims disabled for it, by the trim element in force (b_disable_trim per object,
    /// or global_trim_mode 1). Carried with the trims to payloads that do not restate them.
    signalled_bypass: [bool; MAX_CHANNELS],
    /// Each element's divergence at the last block scheduled, for an object_div_mode 1 (reuse the
    /// previous block's), which the parser can only resolve from a value its own payload codes.
    divergence: [Option<f32>; MAX_CHANNELS],
    /// Payloads whose EMDF container states a sample offset other than their own (see `ingest`).
    pub offset_mismatches: u64,
    /// The shortest interval between payloads, in frames, and the rate it was measured at: one frame
    /// of metadata, which says which rate the payloads count their samples at (`timing_units`). A
    /// property of the stream, so it outlives `clear` (a seek) and goes with `forget_timing`.
    shortest_interval: Option<(u64, u64)>,
    last_payload: Option<u64>,
    /// A payload read before the rate its timing counts in was known, waiting for the next.
    deferred: Option<Box<(ObjectAudioMetadataPayload, u64, Option<u64>)>>,
    current: Box<Elements>,
    pending: VecDeque<Pending>,
    block_elements: Vec<Elements>,
    /// Each of this block's updates' trims and warp mode, parallel to `block_elements`.
    block_trims: Vec<([Trim; TRIM_CONFIGS], i32)>,
    block_updates: Vec<MetadataUpdate>,
    metadata: Metadata,
    bed_labels: Vec<usize>,
}

pub struct BlockMetadata {
    pub metadata: *const Metadata,
    pub update_count: usize,
}

impl Default for ObjectState {
    fn default() -> Self {
        Self {
            have_state: false,
            layout: None,
            warp_mode: WARP_NOT_SIGNALLED,
            trims: [Trim::DEFAULT; TRIM_CONFIGS],
            signalled_warp_mode: WARP_NOT_SIGNALLED,
            signalled_trims: [Trim::DEFAULT; TRIM_CONFIGS],
            signalled_bypass: [false; MAX_CHANNELS],
            divergence: [None; MAX_CHANNELS],
            offset_mismatches: 0,
            shortest_interval: None,
            last_payload: None,
            deferred: None,
            current: Box::new([Element::EMPTY; MAX_CHANNELS]),
            pending: VecDeque::with_capacity(MAX_PENDING),
            block_elements: Vec::with_capacity(MAX_UPDATES_PER_BLOCK),
            block_trims: Vec::with_capacity(MAX_UPDATES_PER_BLOCK),
            block_updates: Vec::with_capacity(MAX_UPDATES_PER_BLOCK),
            metadata: Metadata {
                element_count: 0,
                bed_count: 0,
                isf_count: 0,
                dynamic_count: 0,
                warp_mode: WARP_NOT_SIGNALLED,
                update_count: 0,
                updates: std::ptr::null(),
                elements: std::ptr::null(),
            },
            bed_labels: Vec::with_capacity(MAX_CHANNELS + 1),
        }
    }
}

impl ObjectState {
    /// A different stream: its payloads may count their timing at another rate.
    pub fn forget_timing(&mut self) {
        self.shortest_interval = None;
    }

    /// The element configuration of the updates delivered so far, if any since reset.
    pub fn layout(&self) -> Option<Layout> {
        self.layout.filter(|_| self.have_state)
    }

    pub fn clear(&mut self) {
        self.have_state = false;
        self.layout = None;
        self.warp_mode = WARP_NOT_SIGNALLED;
        self.trims = [Trim::DEFAULT; TRIM_CONFIGS];
        self.signalled_warp_mode = WARP_NOT_SIGNALLED;
        self.signalled_trims = [Trim::DEFAULT; TRIM_CONFIGS];
        self.signalled_bypass = [false; MAX_CHANNELS];
        self.divergence = [None; MAX_CHANNELS];
        self.last_payload = None;
        self.deferred = None;
        self.pending.clear();
        self.block_elements.clear();
        self.block_trims.clear();
        self.block_updates.clear();
    }

    /// Schedules the updates of one payload that arrived with the access unit starting at
    /// stream frame `au_start`, in an EMDF payload stating `evo_offset` (smploffst), if any.
    ///
    /// The payload's own sample_offset times its updates. TS 103 420 5.3 defines it as the
    /// offset TS 102 366 H.2.2.3.2 gives smploffst, to the first sample the payload applies to
    /// from the start of the frame, so the two state one offset and are not added. A container
    /// that states another is counted in
    /// `offset_mismatches`, and the payload's own is used.
    ///
    /// The payload's timing counts samples at a rate it does not state (`timing_units`). At 48 or
    /// 44.1 kHz that can only be the stream's own; at a multiple of them the interval between
    /// payloads says which, so a stream's first payload waits for its second. A seek within the
    /// stream keeps what was measured.
    pub fn ingest(
        &mut self,
        payload: &ObjectAudioMetadataPayload,
        au_start: u64,
        evo_offset: Option<u64>,
        channel_count: usize,
        sample_rate: u32,
    ) -> Result<(), String> {
        let rate = sample_rate as u64;
        if payload.object_element.is_none() {
            return self.schedule(payload, au_start, evo_offset, channel_count, rate, rate);
        }
        if self.shortest_interval.is_some_and(|(_, measured_at)| measured_at != rate) {
            self.shortest_interval = None;
        }
        if let Some(last) = self.last_payload.filter(|&l| au_start > l) {
            let interval = au_start - last;
            let shortest = self.shortest_interval.map_or(interval, |(s, _)| s.min(interval));
            self.shortest_interval = Some((shortest, rate));
        }
        self.last_payload = Some(au_start);
        let Some(units) = self.timing_units(rate) else {
            self.deferred = Some(Box::new((payload.clone(), au_start, evo_offset)));
            return Ok(());
        };
        if let Some(deferred) = self.deferred.take() {
            let (payload, au_start, evo_offset) = *deferred;
            self.schedule(&payload, au_start, evo_offset, channel_count, rate, units)?;
        }
        self.schedule(payload, au_start, evo_offset, channel_count, rate, units)
    }

    /// The rate a payload's timing counts samples at, `None` while it cannot be known yet. One
    /// frame of metadata is 1536 of them (TS 103 420 5.3), so the interval between payloads gives
    /// it: 1536 times the stream's rate over that interval, snapped to the stream's base rate (48 or
    /// 44.1 kHz) times 1, 2 or 4. The shortest interval since reset is the one used, as a payload
    /// lost to damage only lengthens one. Atmos is authored at 48 kHz, and on every stream measured
    /// (all 48 kHz) a payload comes every 1536 frames.
    fn timing_units(&self, rate: u64) -> Option<u64> {
        let base = if rate % 44_100 == 0 { 44_100 } else { 48_000 };
        if rate <= base {
            return Some(rate);
        }
        let measured = OAMD_FRAME * rate / self.shortest_interval?.0;
        [base, 2 * base, 4 * base]
            .into_iter()
            .filter(|&u| u <= rate)
            .min_by_key(|&u| u.abs_diff(measured))
    }

    fn schedule(
        &mut self,
        payload: &ObjectAudioMetadataPayload,
        au_start: u64,
        evo_offset: Option<u64>,
        channel_count: usize,
        sample_rate: u64,
        units: u64,
    ) -> Result<(), String> {
        if let Some(trim) = &payload.trim_element {
            self.signalled_warp_mode = trim.warp_mode as i32;
            self.signalled_trims = trims(trim);
            for (i, bypass) in self.signalled_bypass.iter_mut().enumerate() {
                *bypass = if trim.b_disable_trim_per_obj {
                    trim.b_disable_trim.get(i).copied().unwrap_or(false)
                } else {
                    trim.global_trim_mode == 1
                };
            }
        }
        let Some(object_element) = &payload.object_element else {
            // A payload without object data has no update to time its trims by. TS 103 448 gives
            // trims per metadata update (table 2), so they apply from the next update, which takes
            // them up as the trims in force; updates already scheduled keep their own.
            return Ok(());
        };

        let prog = &payload.program_assignment;
        let count = payload.object_count;
        if count != channel_count || count > MAX_CHANNELS {
            return Err(format!(
                "OAMD describes {count} elements but the presentation decodes {channel_count} channels"
            ));
        }
        self.bed_labels.clear();
        for bed in &prog.bed_assignment {
            self.bed_labels.extend(bed.to_index_vec());
        }
        let bed_count = prog.num_bed_objects;
        let isf_count = prog.num_isf_objects;
        if self.bed_labels.len() != bed_count
            || bed_count + isf_count + prog.num_dynamic_objects != count
        {
            return Err(format!(
                "OAMD program assignment inconsistent: {} labels, {bed_count} bed objects, {isf_count} ISF, {} dynamic, {count} elements",
                self.bed_labels.len(),
                prog.num_dynamic_objects
            ));
        }

        let mut layout = Layout {
            element_count: count,
            bed_count,
            isf_count,
            dynamic_count: count - bed_count - isf_count,
            speakers: [SPEAKER_UNKNOWN; MAX_CHANNELS],
        };
        for (i, speaker) in layout.speakers.iter_mut().enumerate().take(count) {
            *speaker = if i < bed_count {
                speaker_of_oamd_bed(self.bed_labels[i])
            } else {
                SPEAKER_OBJECT
            };
        }

        let info = &object_element.md_update_info;
        if evo_offset.is_some_and(|o| o != info.sample_offset as u64) {
            self.offset_mismatches += 1;
        }
        let frames = |samples: u64| (samples * sample_rate + units / 2) / units;
        let blocks = info.num_obj_info_blocks;
        if info.block_update_info.len() != blocks
            || object_element.object_data.len() != count
            || object_element.object_data.iter().any(|o| o.len() != blocks)
        {
            return Err("OAMD object data does not match its update count".into());
        }

        for (blk, update) in info.block_update_info.iter().enumerate() {
            // ETSI TS 103 420 clause 5.3: t = sample_offset + 32 * block_offset_factor.
            let due = au_start + frames(info.sample_offset as u64 + 32 * update.block_offset_factor_bits as u64);
            let mut elements = [Element::EMPTY; MAX_CHANNELS];
            for (i, element) in elements.iter_mut().enumerate().take(count) {
                *element = self.element(payload, &layout, i, blk);
            }
            for (i, element) in elements.iter().enumerate().take(count) {
                self.divergence[i] = (element.flags & EL_DIVERGENCE != 0).then_some(element.divergence);
            }
            if self.pending.len() >= MAX_PENDING {
                return Err("too many pending OAMD updates".into());
            }
            let at = self.pending.partition_point(|p| p.due <= due);
            self.pending.insert(
                at,
                Pending {
                    due,
                    ramp: frames(update.ramp_duration as u64) as u32,
                    layout,
                    elements,
                    warp_mode: self.signalled_warp_mode,
                    trims: self.signalled_trims,
                },
            );
        }
        Ok(())
    }

    fn element(
        &self,
        p: &ObjectAudioMetadataPayload,
        layout: &Layout,
        i: usize,
        blk: usize,
    ) -> Element {
        let od = &p.object_element.as_ref().expect("checked by caller").object_data[i][blk];
        let mut e = Element::EMPTY;

        if i < layout.bed_count {
            let label = self.bed_labels[i];
            e.speaker = layout.speakers[i];
            e.kind = if e.speaker == SPEAKER_LFE || e.speaker == SPEAKER_LFE2 {
                ELEMENT_LFE
            } else {
                ELEMENT_BED
            };
            e.position = oamd_bed_position(label);
        } else if i < layout.bed_count + layout.isf_count {
            e.kind = ELEMENT_ISF;
        } else {
            e.kind = ELEMENT_OBJECT;
        }

        if !od.b_object_not_active {
            e.flags |= EL_ACTIVE;
        }
        let basic = &od.object_basic_info;
        if basic.object_gain == GAIN_MINUS_INFINITY {
            e.gain = 0.0;
            e.gain_db = f32::NEG_INFINITY;
        } else {
            e.gain_db = basic.object_gain as f32;
            e.gain = 10f32.powf(e.gain_db / 20.0);
        }
        e.priority = basic.object_priority as f32;

        if e.kind == ELEMENT_OBJECT {
            let r = &od.object_render_info;
            let mut pos = r.pos3d;
            if let Some(ext) = p
                .extended_object_element
                .as_ref()
                .and_then(|x| x.ext_prec_pos_block.get(i))
                .and_then(|o| o.get(blk))
            {
                pos[0] += ext.ext_prec_pos3d_x;
                pos[1] += ext.ext_prec_pos3d_y;
                pos[2] += ext.ext_prec_pos3d_z;
            }
            e.position = [
                pos[0].clamp(0.0, 1.0) as f32,
                pos[1].clamp(0.0, 1.0) as f32,
                pos[2].clamp(-1.0, 1.0) as f32,
            ];
            e.zone = r.zone_constraints_idx;
            if r.b_enable_elevation {
                e.flags |= EL_ELEVATION;
            }
            if r.b_object_snap {
                e.flags |= EL_SNAP;
            }
            e.size = r.object_size.map(|s| s as f32);
            if r.b_object_use_screen_ref {
                e.flags |= EL_SCREEN_REF;
                e.screen_factor = r.screen_factor as f32;
            }
            e.depth_factor = r.depth_factor as f32;
            if r.b_object_distance_specified {
                e.flags |= EL_DISTANCE;
                e.distance = r.distance_factor.unwrap_or(0.0) as f32;
            }
            if let Some(div) = p
                .extended_object_element
                .as_ref()
                .filter(|x| x.b_obj_div_block)
                .and_then(|x| x.object_div_block.get(i))
                .and_then(|o| o.get(blk))
                .filter(|d| d.b_object_divergence)
            {
                // Mode 1 reuses the previous block's value, the one this element was last given:
                // at block 0 in the last payload, and at any block whose predecessor reused too.
                let divergence = if div.object_div_mode == 1 {
                    self.divergence[i]
                } else {
                    Some(div.object_divergence as f32)
                };
                if let Some(divergence) = divergence {
                    e.flags |= EL_DIVERGENCE;
                    e.divergence = divergence;
                }
            }
        }

        if self.signalled_bypass[i] {
            e.flags |= EL_TRIM_BYPASS;
        }
        if let Some(hp) = p
            .headphone_element
            .as_ref()
            .and_then(|h| h.headphone_data.get(i))
            .and_then(|o| o.get(blk.min(o.len().saturating_sub(1))))
        {
            e.headphone_mode = hp.hp_render_mode;
            if hp.hp_head_track_disable {
                e.flags |= EL_HEAD_TRACK_DISABLE;
            }
        }
        if let Some(d) = p.object_description_element.as_ref() {
            let at = |o: &Vec<u8>| o.get(blk.min(o.len().saturating_sub(1))).copied();
            if let Some(dialog) = d.object_dialog_indication.get(i).and_then(at) {
                e.dialog = dialog;
            }
            if let Some(Some(extra)) = d.object_dialog_extra.get(i).and_then(|o| o.get(blk.min(o.len().saturating_sub(1)))) {
                e.dialog_extra = *extra;
            }
        }
        e
    }

    /// The trim configurations of the latest update delivered (all default before any).
    pub fn trims(&self) -> [Trim; TRIM_CONFIGS] {
        self.trims
    }

    /// Moves the updates starting within `[start, start + frames)` into this block and returns
    /// the metadata to attach, or a null pointer before the first update since reset.
    pub fn begin_block(&mut self, start: u64, frames: u32) -> BlockMetadata {
        self.block_elements.clear();
        self.block_trims.clear();
        self.block_updates.clear();
        let end = start + frames as u64;
        let mut offsets: [(u32, u32); MAX_UPDATES_PER_BLOCK] = [(0, 0); MAX_UPDATES_PER_BLOCK];

        while self.pending.front().is_some_and(|p| p.due < end) {
            let p = self.pending.pop_front().expect("front checked");
            // Trims and warp take effect with their update; a block reports the last one's.
            self.warp_mode = p.warp_mode;
            self.trims = p.trims;
            let count = p.layout.element_count;
            let mut elements = p.elements;
            let layout_changed = self.layout != Some(p.layout);
            for (e, cur) in elements.iter_mut().zip(self.current.iter()).take(count) {
                e.flags &= !EL_CHANGED;
                if !self.have_state || layout_changed || !e.same_values(cur) {
                    e.flags |= EL_CHANGED;
                }
            }
            if layout_changed {
                self.layout = Some(p.layout);
            }
            *self.current = elements;
            for e in self.current.iter_mut() {
                e.flags &= !EL_CHANGED;
            }
            self.have_state = true;
            offsets[self.block_elements.len()] = (p.due.saturating_sub(start) as u32, p.ramp);
            self.block_elements.push(elements);
            self.block_trims.push((p.trims, p.warp_mode));
        }

        // Pointers are taken only now that block_elements and block_trims will not grow again.
        for (i, (elements, (trims, warp_mode))) in self.block_elements.iter().zip(&self.block_trims).enumerate() {
            self.block_updates.push(MetadataUpdate {
                frame_offset: offsets[i].0,
                ramp_frames: offsets[i].1,
                elements: elements.as_ptr(),
                trims: trims.as_ptr(),
                warp_mode: *warp_mode,
                reserved: 0,
            });
        }

        let Some(layout) = self.layout.filter(|_| self.have_state) else {
            return BlockMetadata {
                metadata: std::ptr::null(),
                update_count: 0,
            };
        };
        self.metadata = Metadata {
            element_count: layout.element_count as u32,
            bed_count: layout.bed_count as u32,
            isf_count: layout.isf_count as u32,
            dynamic_count: layout.dynamic_count as u32,
            warp_mode: self.warp_mode,
            update_count: self.block_updates.len() as u32,
            updates: if self.block_updates.is_empty() {
                std::ptr::null()
            } else {
                self.block_updates.as_ptr()
            },
            elements: self.current.as_ptr(),
        };
        BlockMetadata {
            metadata: &self.metadata,
            update_count: self.block_updates.len(),
        }
    }
}

/// TS 103 420 5.6.5 as TS 103 448 table 7 reads it: a global mode, which a configuration's
/// b_default_trim and b_disable_trim override; a custom trim's gains, 0 dB where not sent.
fn trims(trim: &TrimElement) -> [Trim; TRIM_CONFIGS] {
    let global = match trim.global_trim_mode {
        1 => TRIM_DISABLED,
        2 => TRIM_CUSTOM,
        _ => TRIM_DEFAULT, // 0, and 3 (reserved)
    };
    let mut out = [Trim { mode: global, ..Trim::DEFAULT }; TRIM_CONFIGS];
    if global != TRIM_CUSTOM {
        return out;
    }
    for (o, t) in out.iter_mut().zip(trim.trims.iter()) {
        let Some(t) = t else { continue };
        *o = if t.b_default_trim {
            Trim::DEFAULT
        } else if t.b_disable_trim {
            Trim { mode: TRIM_DISABLED, ..Trim::DEFAULT }
        } else {
            Trim {
                mode: TRIM_CUSTOM,
                reserved: [0; 3],
                centre_db: t.trim_centre.unwrap_or(0.0) as f32,
                surround_db: t.trim_surround.unwrap_or(0.0) as f32,
                height_db: t.trim_height.unwrap_or(0.0) as f32,
                balance_top: t.bal3d_y_tb.unwrap_or(0.0) as f32,
                balance_listener: t.bal3d_y_lis.unwrap_or(0.0) as f32,
            }
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use truehd::structs::oamd::Trim as OamdTrim;

    /// A payload carrying one dynamic object, one update at its start, and a trim element.
    fn payload(warp_mode: u8, global_trim_mode: u8) -> ObjectAudioMetadataPayload {
        use truehd::structs::oamd::*;
        ObjectAudioMetadataPayload {
            object_count: 1,
            program_assignment: ProgramAssignment { num_dynamic_objects: 1, ..Default::default() },
            object_element: Some(ObjectElement {
                md_update_info: MDUpdateInfo {
                    sample_offset: 0,
                    num_obj_info_blocks: 1,
                    block_update_info: vec![BlockUpdateInfo::default()],
                },
                object_data: vec![vec![ObjectInfoBlock::default()]],
                ..Default::default()
            }),
            trim_element: Some(TrimElement { warp_mode, global_trim_mode, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn trims_and_warp_take_effect_with_their_update() {
        let mut state = ObjectState::default();
        state.ingest(&payload(0, 0), 0, None, 1, 48_000).unwrap();
        state.begin_block(0, 40);
        // Read early, as payloads are: the next update is due 4000 frames later.
        state.ingest(&payload(1, 1), 4000, None, 1, 48_000).unwrap();
        state.begin_block(40, 40);
        assert_eq!(state.metadata.warp_mode, 0);
        assert_eq!(state.trims()[8].mode, TRIM_DEFAULT);
        state.begin_block(4000, 40);
        assert_eq!(state.metadata.warp_mode, 1);
        assert_eq!(state.trims()[8].mode, TRIM_DISABLED);
    }

    /// Two updates in one block each carry their own trims and warp mode.
    #[test]
    fn each_update_carries_its_trims() {
        let mut state = ObjectState::default();
        state.ingest(&payload(0, 0), 0, None, 1, 48_000).unwrap();
        state.ingest(&payload(1, 1), 20, None, 1, 48_000).unwrap();
        let block = state.begin_block(0, 40);
        assert_eq!(block.update_count, 2);
        let updates = unsafe { std::slice::from_raw_parts(state.metadata.updates, 2) };
        let first = unsafe { std::slice::from_raw_parts(updates[0].trims, TRIM_CONFIGS) };
        let second = unsafe { std::slice::from_raw_parts(updates[1].trims, TRIM_CONFIGS) };
        assert_eq!((updates[0].warp_mode, first[8].mode), (0, TRIM_DEFAULT));
        assert_eq!((updates[1].warp_mode, second[8].mode), (1, TRIM_DISABLED));
    }

    /// object_div_mode 1 takes the value the element was last given, however many blocks in a
    /// row reuse it, across payloads.
    #[test]
    fn divergence_reuse_chains() {
        use truehd::structs::oamd::*;
        let with_divergence = |blocks: usize, modes: &[(u8, f64)]| {
            let mut p = payload(0, 0);
            let element = p.object_element.as_mut().unwrap();
            element.md_update_info.num_obj_info_blocks = blocks;
            element.md_update_info.block_update_info = (0..blocks)
                .map(|b| BlockUpdateInfo { block_offset_factor_bits: 4 * b as u8, ..Default::default() })
                .collect();
            element.object_data = vec![vec![ObjectInfoBlock::default(); blocks]];
            p.extended_object_element = Some(ExtendedObjectElement {
                b_obj_div_block: true,
                object_div_block: vec![modes
                    .iter()
                    .map(|&(object_div_mode, object_divergence)| ObjectDivergenceBlock {
                        b_object_divergence: true,
                        object_div_mode,
                        // What the parser leaves for a reuse it cannot resolve.
                        object_divergence: if object_div_mode == 1 { 0.0 } else { object_divergence },
                        ..Default::default()
                    })
                    .collect()],
                ..Default::default()
            });
            p
        };
        let mut state = ObjectState::default();
        state.ingest(&with_divergence(1, &[(0, 0.5)]), 0, None, 1, 48_000).unwrap();
        state.ingest(&with_divergence(3, &[(1, 0.0), (1, 0.0), (0, 0.25)]), 1536, None, 1, 48_000).unwrap();
        let divergences: Vec<f32> = state.pending.iter().map(|p| p.elements[0].divergence).collect();
        assert_eq!(divergences, [0.5, 0.5, 0.5, 0.25]);
    }

    /// At 96 kHz the interval between payloads says which rate their timing counts at, and the first
    /// payload waits for the second to say it.
    #[test]
    fn timing_at_96k() {
        use truehd::structs::oamd::*;
        let timed = || {
            let mut p = payload(0, 0);
            let element = p.object_element.as_mut().unwrap();
            element.md_update_info.sample_offset = 16;
            element.md_update_info.block_update_info =
                vec![BlockUpdateInfo { block_offset_factor_bits: 2, ramp_duration: 512, ..Default::default() }];
            p
        };
        let scheduled = |state: &ObjectState| state.pending.iter().map(|p| (p.due, p.ramp)).collect::<Vec<_>>();

        // A payload every 3072 frames: 48 kHz samples, doubled.
        let mut state = ObjectState::default();
        state.ingest(&timed(), 0, None, 1, 96_000).unwrap();
        assert!(state.pending.is_empty(), "the first payload waits");
        state.ingest(&timed(), 3072, None, 1, 96_000).unwrap();
        assert_eq!(scheduled(&state), [(160, 1024), (3072 + 160, 1024)]);

        // A payload every 1536 frames, with one lost: the stream's own samples.
        let mut state = ObjectState::default();
        for au in [0, 3072, 4608] {
            state.ingest(&timed(), au, None, 1, 96_000).unwrap();
        }
        assert_eq!(scheduled(&state).last(), Some(&(4608 + 80, 512)));

        // A seek keeps what the stream's payloads said: the next first payload is scheduled at once.
        let mut state = ObjectState::default();
        for au in [0, 3072] {
            state.ingest(&timed(), au, None, 1, 96_000).unwrap();
        }
        state.clear();
        state.ingest(&timed(), 0, None, 1, 96_000).unwrap();
        assert_eq!(scheduled(&state), [(160, 1024)]);
        // Another stream, or another rate, measures again.
        state.clear();
        state.forget_timing();
        state.ingest(&timed(), 0, None, 1, 96_000).unwrap();
        assert!(state.pending.is_empty());
        state.clear();
        state.ingest(&timed(), 0, None, 1, 192_000).unwrap();
        assert!(state.pending.is_empty());

        // At 192 kHz a payload every 6144 frames: 48 kHz samples, four times over.
        let mut state = ObjectState::default();
        for au in [0, 6144] {
            state.ingest(&timed(), au, None, 1, 192_000).unwrap();
        }
        assert_eq!(scheduled(&state), [(320, 2048), (6144 + 320, 2048)]);

        // At 48 kHz, at once and as stated.
        let mut state = ObjectState::default();
        state.ingest(&timed(), 0, None, 1, 48_000).unwrap();
        assert_eq!(scheduled(&state), [(80, 512)]);
    }

    /// A payload without a trim element keeps the last one's per-object bypass, as it keeps its trims.
    #[test]
    fn trim_bypass_carries_over() {
        let mut first = payload(0, 2);
        let trim = first.trim_element.as_mut().unwrap();
        trim.b_disable_trim_per_obj = true;
        trim.b_disable_trim = vec![true];
        let mut later = payload(0, 0);
        later.trim_element = None;
        let mut state = ObjectState::default();
        state.ingest(&first, 0, None, 1, 48_000).unwrap();
        state.ingest(&later, 1536, None, 1, 48_000).unwrap();
        assert!(state.pending.iter().all(|p| p.elements[0].flags & EL_TRIM_BYPASS != 0));
        assert_eq!(state.pending.back().unwrap().trims[0].mode, TRIM_CUSTOM);
    }

    /// Payloads more often than once a frame can put more than two payloads' updates in one block:
    /// each still gets its own entry, at its own frame.
    #[test]
    fn a_crowded_block_keeps_every_update() {
        use truehd::structs::oamd::*;
        let eight = || {
            let mut p = payload(0, 0);
            let element = p.object_element.as_mut().unwrap();
            element.md_update_info.num_obj_info_blocks = 8;
            element.md_update_info.block_update_info = vec![BlockUpdateInfo::default(); 8];
            element.object_data = vec![vec![ObjectInfoBlock::default(); 8]];
            p
        };
        let mut state = ObjectState::default();
        for au in [0, 10, 20] {
            state.ingest(&eight(), au, None, 1, 48_000).unwrap();
        }
        let block = state.begin_block(0, 40);
        assert_eq!(block.update_count, 24);
        let updates = unsafe { std::slice::from_raw_parts(state.metadata.updates, 24) };
        assert_eq!(updates.iter().map(|u| u.frame_offset).collect::<Vec<_>>(),
                   [[0; 8], [10; 8], [20; 8]].concat());
    }

    /// A payload with trims and no object data has no update of its own: its trims are in force from
    /// the next update on, and the block before that keeps reporting the last update's.
    #[test]
    fn trims_alone_wait_for_the_next_update() {
        let mut state = ObjectState::default();
        state.ingest(&payload(0, 0), 0, None, 1, 48_000).unwrap();
        state.begin_block(0, 40);
        let mut trims_only = payload(1, 1);
        trims_only.object_element = None;
        state.ingest(&trims_only, 40, None, 1, 48_000).unwrap();
        state.begin_block(40, 40);
        assert_eq!((state.metadata.warp_mode, state.trims()[0].mode), (0, TRIM_DEFAULT));
        let mut next = payload(0, 0);
        next.trim_element = None;
        state.ingest(&next, 1536, None, 1, 48_000).unwrap();
        state.begin_block(1536, 40);
        assert_eq!((state.metadata.warp_mode, state.trims()[0].mode), (1, TRIM_DISABLED));
    }

    #[test]
    fn trim_modes() {
        let mut element = TrimElement { global_trim_mode: 1, ..Default::default() };
        assert!(trims(&element).iter().all(|t| t.mode == TRIM_DISABLED));

        element.global_trim_mode = 2;
        element.trims[0] = Some(OamdTrim { b_default_trim: true, ..OamdTrim::default() });
        element.trims[1] = Some(OamdTrim { b_default_trim: false, b_disable_trim: true, ..OamdTrim::default() });
        element.trims[8] = Some(OamdTrim {
            b_default_trim: false,
            trim_surround: Some(-3.0),
            trim_height: Some(-6.0),
            ..OamdTrim::default()
        });
        let t = trims(&element);
        assert_eq!(t[0], Trim::DEFAULT);
        assert_eq!(t[1].mode, TRIM_DISABLED);
        assert_eq!((t[8].mode, t[8].centre_db, t[8].surround_db, t[8].height_db), (TRIM_CUSTOM, 0.0, -3.0, -6.0));
        // Not transmitted in a custom-trim element: custom, at 0 dB.
        assert_eq!((t[4].mode, t[4].height_db), (TRIM_CUSTOM, 0.0));
    }
}
