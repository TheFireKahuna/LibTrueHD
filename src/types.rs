//! `#[repr(C)]` mirrors of the types in `include/truehd_atmos.h`. The header is the contract;
//! the size and offset assertions at the bottom keep the two from drifting apart.

use std::mem::{offset_of, size_of};

pub const API_VERSION: u32 = 2;
pub const MAX_CHANNELS: usize = 16;
pub const MAX_BLOCK_FRAMES: usize = 160;
pub const NO_PTS: i64 = i64::MIN;

pub type Status = i32;
pub const OK: Status = 0;
pub const NEED_MORE_DATA: Status = 1;
pub const ERR_NULL: Status = -1;
pub const ERR_INVALID_ARGUMENT: Status = -2;
pub const ERR_NOT_READY: Status = -3;
pub const ERR_BUFFER_FULL: Status = -4;
pub const ERR_PANIC: Status = -5;

pub const PRESENTATION_HIGHEST: i32 = -1;
pub const PRESENTATION_HIGHEST_CHANNEL_BASED: i32 = -2;

pub const SPEAKER_LFE: u8 = 3;
pub const SPEAKER_LFE2: u8 = 23;
pub const SPEAKER_OBJECT: u8 = 254;
pub const SPEAKER_UNKNOWN: u8 = 255;

pub const ELEMENT_BED: u8 = 0;
pub const ELEMENT_LFE: u8 = 1;
pub const ELEMENT_OBJECT: u8 = 2;
pub const ELEMENT_ISF: u8 = 3;

pub const EL_ACTIVE: u32 = 1 << 0;
pub const EL_CHANGED: u32 = 1 << 1;
pub const EL_SNAP: u32 = 1 << 2;
pub const EL_ELEVATION: u32 = 1 << 3;
pub const EL_SCREEN_REF: u32 = 1 << 4;
pub const EL_DISTANCE: u32 = 1 << 5;
pub const EL_DIVERGENCE: u32 = 1 << 6;
pub const EL_TRIM_BYPASS: u32 = 1 << 7;
pub const EL_HEAD_TRACK_DISABLE: u32 = 1 << 8;

pub const WARP_NOT_SIGNALLED: i32 = -1;

pub const BLOCK_DISCONTINUITY: u32 = 1 << 0;
pub const BLOCK_LAYOUT_CHANGED: u32 = 1 << 1;
pub const BLOCK_HAS_METADATA: u32 = 1 << 2;
pub const BLOCK_METADATA_UPDATED: u32 = 1 << 3;
pub const BLOCK_INPUT_OFFSET_ESTIMATED: u32 = 1 << 4;

pub const FORMAT_TRUEHD: u32 = 0;
pub const FORMAT_MLP: u32 = 1;

pub const PTYPE_ABSENT: i32 = 0;
pub const PTYPE_INDEPENDENT: i32 = 1;
pub const PTYPE_DOWNMIX: i32 = 2;
pub const PTYPE_COPY: i32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Element {
    pub kind: u8,
    pub speaker: u8,
    pub zone: u8,
    pub headphone_mode: u8,
    pub flags: u32,
    pub position: [f32; 3],
    pub gain: f32,
    pub gain_db: f32,
    pub size: [f32; 3],
    pub divergence: f32,
    pub priority: f32,
    pub distance: f32,
    pub screen_factor: f32,
    pub depth_factor: f32,
    pub dialog: u8,
    pub reserved: [u8; 3],
}

impl Element {
    pub const EMPTY: Self = Self {
        kind: ELEMENT_OBJECT,
        speaker: SPEAKER_OBJECT,
        zone: 0,
        headphone_mode: 255,
        flags: 0,
        position: [0.0; 3],
        gain: 0.0,
        gain_db: f32::NEG_INFINITY,
        size: [0.0; 3],
        divergence: 0.0,
        priority: 0.0,
        distance: 0.0,
        screen_factor: 0.0,
        depth_factor: 0.0,
        dialog: 255,
        reserved: [0; 3],
    };

    /// Equal in everything a renderer acts on, ignoring the CHANGED marker itself.
    pub fn same_values(&self, other: &Self) -> bool {
        let mut a = *self;
        let mut b = *other;
        a.flags &= !EL_CHANGED;
        b.flags &= !EL_CHANGED;
        a == b
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MetadataUpdate {
    pub frame_offset: u32,
    pub ramp_frames: u32,
    pub elements: *const Element,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub element_count: u32,
    pub bed_count: u32,
    pub isf_count: u32,
    pub dynamic_count: u32,
    pub warp_mode: i32,
    pub update_count: u32,
    pub updates: *const MetadataUpdate,
    pub elements: *const Element,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub sample_rate: u32,
    pub frame_count: u32,
    pub channel_count: u32,
    pub presentation: u32,
    pub flags: u32,
    pub access_unit_frames: u32,
    pub pts: i64,
    pub pts_au_index: u32,
    pub layout_serial: u32,
    pub sample_position: u64,
    pub input_access_unit: u64,
    pub input_frame_offset: u64,
    pub channels: *const *const f32,
    pub speakers: *const u8,
    pub metadata: *const Metadata,
    pub bed_channel_count: u32,
    pub bed_channels: *const *const f32,
    pub bed_speakers: *const u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationInfo {
    pub ptype: i32,
    pub source: i32,
    pub channel_count: u32,
    pub speakers: [u8; MAX_CHANNELS],
}

impl PresentationInfo {
    pub const ABSENT: Self = Self {
        ptype: PTYPE_ABSENT,
        source: -1,
        channel_count: 0,
        speakers: [SPEAKER_UNKNOWN; MAX_CHANNELS],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub format: u32,
    pub sample_rate: u32,
    pub access_unit_frames: u32,
    pub substream_count: u32,
    pub selected_presentation: u32,
    pub immersive: u32,
    pub has_objects: u32,
    pub oamd_seen: u32,
    pub layout_serial: u32,
    pub presentations: [PresentationInfo; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub bytes_pushed: u64,
    pub bytes_pending: u64,
    pub access_units_decoded: u64,
    pub blocks_output: u64,
    pub frames_output: u64,
    pub access_units_skipped: u64,
    pub duplicate_access_units: u64,
    pub decode_errors: u64,
    pub sync_errors: u64,
    pub oamd_payloads: u64,
    pub oamd_errors: u64,
    pub metadata_updates: u64,
}

// Layout checks against the header (arm64 and x86_64 Apple ABIs, 8-byte pointers).
const _: () = {
    assert!(size_of::<Element>() == 64);
    assert!(offset_of!(Element, flags) == 4);
    assert!(offset_of!(Element, position) == 8);
    assert!(offset_of!(Element, gain) == 20);
    assert!(offset_of!(Element, size) == 28);
    assert!(offset_of!(Element, divergence) == 40);
    assert!(offset_of!(Element, dialog) == 60);
    assert!(size_of::<MetadataUpdate>() == 16);
    assert!(size_of::<Metadata>() == 40);
    assert!(offset_of!(Metadata, updates) == 24);
    assert!(size_of::<Block>() == 112);
    assert!(offset_of!(Block, pts) == 24);
    assert!(offset_of!(Block, layout_serial) == 36);
    assert!(offset_of!(Block, input_frame_offset) == 56);
    assert!(offset_of!(Block, channels) == 64);
    assert!(offset_of!(Block, metadata) == 80);
    assert!(offset_of!(Block, bed_channel_count) == 88);
    assert!(offset_of!(Block, bed_channels) == 96);
    assert!(offset_of!(Block, bed_speakers) == 104);
    assert!(size_of::<PresentationInfo>() == 28);
    assert!(size_of::<StreamInfo>() == 36 + 4 * 28);
    assert!(size_of::<Stats>() == 12 * 8);
};
