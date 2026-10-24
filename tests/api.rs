//! Behaviour of the C API that the sample harness does not reach: argument handling,
//! presentation switching, back-pressure, and damaged input. Runs in-process through the same
//! `extern "C"` functions the xcframework exports.

use std::path::PathBuf;
use std::ptr;
use truehd_atmos::types::*;
use truehd_atmos::*;

/// The working directory `harness/verify.sh` fills: `$TRUEHD_ATMOS_WORK`, default `.work/`.
fn work_dir() -> PathBuf {
    std::env::var_os("TRUEHD_ATMOS_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work"))
}

fn asset(name: &str) -> Vec<u8> {
    let path = work_dir().join("truehdd/truehd/tests/assets").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn sample(name: &str) -> Option<Vec<u8>> {
    let path = work_dir().join("samples").join(name);
    std::fs::read(path).ok()
}

fn empty_block() -> Block {
    Block {
        sample_rate: 0,
        frame_count: 0,
        channel_count: 0,
        presentation: 0,
        flags: 0,
        access_unit_frames: 0,
        pts: 0,
        pts_au_index: 0,
        layout_serial: 0,
        sample_position: 0,
        input_access_unit: 0,
        input_frame_offset: 0,
        channels: ptr::null(),
        speakers: ptr::null(),
        metadata: ptr::null(),
        bed_channel_count: 0,
        bed_channels: ptr::null(),
        bed_speakers: ptr::null(),
        input_timing: 0,
        output_timing: 0,
        output_position: 0,
    }
}

struct Decoder(*mut TrueHDAtmosDecoder);

impl Decoder {
    fn new(presentation: i32) -> Self {
        let d = truehd_atmos_decoder_create(presentation);
        assert!(!d.is_null());
        Self(d)
    }

    fn push(&self, data: &[u8], pts: i64) -> Status {
        unsafe { truehd_atmos_decoder_push(self.0, data.as_ptr(), data.len(), pts) }
    }

    /// Pulls every available block, returning (presentation, channels, flags, serial) of each.
    fn drain(&self) -> Vec<(u32, u32, u32, u32)> {
        let mut out = Vec::new();
        loop {
            let mut b = empty_block();
            let st = unsafe { truehd_atmos_decoder_pull(self.0, &mut b) };
            match st {
                OK => {
                    assert!(b.frame_count > 0 && b.frame_count <= 160);
                    assert!(b.channel_count > 0 && b.channel_count <= 16);
                    // Every plane must be readable for frame_count samples, in [-1, 1) where the
                    // decode is exact (without interpolation an object presentation may exceed it).
                    let exact = cfg!(feature = "matrix-interpolation") || b.presentation < 3;
                    for c in 0..b.channel_count as usize {
                        let plane = unsafe {
                            std::slice::from_raw_parts(*b.channels.add(c), b.frame_count as usize)
                        };
                        assert!(plane.iter().all(|v| v.is_finite() && (!exact || (-1.0..1.0).contains(v))));
                    }
                    out.push((b.presentation, b.channel_count, b.flags, b.layout_serial));
                }
                NEED_MORE_DATA => return out,
                other => panic!("pull returned {other}"),
            }
        }
    }

    fn stats(&self) -> Stats {
        let mut s = Stats::default();
        assert_eq!(unsafe { truehd_atmos_decoder_get_stats(self.0, &mut s) }, OK);
        s
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { truehd_atmos_decoder_destroy(self.0) };
    }
}

#[test]
fn arguments_are_checked() {
    assert!(truehd_atmos_decoder_create(4).is_null());
    assert!(truehd_atmos_decoder_create(-3).is_null());
    unsafe {
        truehd_atmos_decoder_destroy(ptr::null_mut());
        assert_eq!(truehd_atmos_decoder_reset(ptr::null_mut()), ERR_NULL);
        assert_eq!(truehd_atmos_decoder_push(ptr::null_mut(), ptr::null(), 4, 0), ERR_NULL);
        let mut b = empty_block();
        assert_eq!(truehd_atmos_decoder_pull(ptr::null_mut(), &mut b), ERR_NULL);
        assert!(!truehd_atmos_decoder_last_error(ptr::null()).is_null());
    }

    let d = Decoder::new(PRESENTATION_HIGHEST);
    unsafe {
        assert_eq!(truehd_atmos_decoder_push(d.0, ptr::null(), 0, 0), OK);
        assert_eq!(truehd_atmos_decoder_push(d.0, ptr::null(), 1, 0), ERR_NULL);
        assert_eq!(truehd_atmos_decoder_pull(d.0, ptr::null_mut()), ERR_NULL);
        assert_eq!(truehd_atmos_decoder_set_presentation(d.0, 7), ERR_INVALID_ARGUMENT);
        let mut info = std::mem::zeroed::<StreamInfo>();
        assert_eq!(truehd_atmos_decoder_get_stream_info(d.0, &mut info), ERR_NOT_READY);
        let mut b = empty_block();
        assert_eq!(truehd_atmos_decoder_pull(d.0, &mut b), NEED_MORE_DATA);
    }
    assert_eq!(unsafe { std::ffi::CStr::from_ptr(truehd_atmos_speaker_name(10)) }, c"Tbl");
    assert_eq!(unsafe { std::ffi::CStr::from_ptr(truehd_atmos_speaker_name(254)) }, c"Obj");
}

#[test]
fn input_beyond_the_limit_is_refused_until_pulled() {
    let d = Decoder::new(PRESENTATION_HIGHEST);
    let chunk = vec![0u8; 1 << 20];
    let mut accepted = 0;
    while d.push(&chunk, NO_PTS) == OK {
        accepted += 1;
        assert!(accepted <= 8, "the 8 MiB limit was not enforced");
    }
    assert_eq!(d.push(&chunk, NO_PTS), ERR_BUFFER_FULL);
    // Pulling scans (and, holding no major sync, lets go of) the garbage.
    assert!(d.drain().is_empty());
    assert_eq!(d.push(&chunk, NO_PTS), OK);
}

#[test]
fn switching_presentation_changes_layout_at_the_next_major_sync() {
    let data = asset("fba_atmos_obj.mlp");
    let d = Decoder::new(PRESENTATION_HIGHEST);
    let half = data.len() / 2;
    assert_eq!(d.push(&data[..half], 0), OK);
    let before = d.drain();
    assert!(!before.is_empty());
    assert!(before.iter().all(|b| b.0 == 3 && b.1 == 16));

    assert_eq!(
        unsafe { truehd_atmos_decoder_set_presentation(d.0, PRESENTATION_HIGHEST_CHANNEL_BASED) },
        OK
    );
    assert_eq!(d.push(&data[half..], 1), OK);
    let after = d.drain();
    // The fixture has one more major sync in its second half.
    assert!(!after.is_empty(), "no blocks after the switch");
    let (p, ch, flags, serial) = after[0];
    assert_eq!((p, ch), (2, 8));
    assert!(flags & BLOCK_DISCONTINUITY != 0 && flags & BLOCK_LAYOUT_CHANGED != 0);
    assert_ne!(serial, before[0].3);
    assert!(after.iter().all(|b| b.0 == 2 && b.3 == serial));
}

#[test]
fn reset_keeps_the_layout_serial_of_the_same_stream() {
    let data = asset("fba_atmos_dimtrim.mlp");
    let d = Decoder::new(PRESENTATION_HIGHEST);
    d.push(&data, 0);
    let first = d.drain();
    assert_eq!(unsafe { truehd_atmos_decoder_reset(d.0) }, OK);
    d.push(&data, 0);
    let second = d.drain();
    assert_eq!(first.len(), second.len());
    assert_eq!(first[0].3, second[0].3);
    assert!(second[0].2 & BLOCK_DISCONTINUITY != 0);
    assert!(second[0].2 & BLOCK_LAYOUT_CHANGED == 0);
}

/// Damaged input must never surface a panic, must be counted, and decoding must continue.
#[test]
fn damaged_input_is_survived() {
    let data = sample("dolby-unfold.thd").unwrap_or_else(|| asset("fba_atmos_dimtrim.mlp"));
    let clean_blocks = {
        let d = Decoder::new(PRESENTATION_HIGHEST);
        d.push(&data, 0);
        d.drain().len()
    };

    let mut state = 0x2545F4914F6CDD1Du64;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..6 {
        let mut damaged = data.clone();
        // Bit flips, zeroed runs and inserted garbage, spread over the stream.
        // Counts sized for the Unfold sample (21 MB); the fixture it falls back to without the
        // samples is 0.3 s, and takes damage in proportion.
        let scale = (data.len() as f64 / 21e6).min(1.0);
        let flips = (([20usize, 200, 2000, 50, 5, 500][round] as f64 * scale).round() as usize).max(1);
        for _ in 0..flips {
            let at = (rand() as usize) % damaged.len();
            match rand() % 3 {
                0 => damaged[at] ^= 1 << (rand() % 8),
                1 => {
                    let end = (at + 64).min(damaged.len());
                    damaged[at..end].fill(0);
                }
                _ => {
                    let junk: Vec<u8> = (0..(rand() % 700)).map(|_| rand() as u8).collect();
                    damaged.splice(at..at, junk);
                }
            }
        }
        let d = Decoder::new(PRESENTATION_HIGHEST);
        let mut blocks = 0;
        for (i, chunk) in damaged.chunks(1 + (rand() % 5000) as usize).enumerate() {
            assert_eq!(d.push(chunk, i as i64), OK);
            blocks += d.drain().len();
        }
        let s = d.stats();
        assert!(s.decode_errors + s.sync_errors > 0, "round {round}: damage went unnoticed");
        assert!(
            blocks * 10 >= clean_blocks * 5,
            "round {round}: only {blocks} of {clean_blocks} blocks survived"
        );
        let message = unsafe { std::ffi::CStr::from_ptr(truehd_atmos_decoder_last_error(d.0)) };
        assert!(!message.to_bytes().is_empty());
    }

    // Pure noise: no blocks, no panic.
    let noise: Vec<u8> = (0..3_000_000).map(|_| rand() as u8).collect();
    let d = Decoder::new(PRESENTATION_HIGHEST);
    for chunk in noise.chunks(65536) {
        assert_eq!(d.push(chunk, NO_PTS), OK);
        assert!(d.drain().is_empty());
    }
}

/// Every presentation of every fixture decodes, and the stream info agrees with the blocks.
#[test]
fn every_presentation_of_every_fixture() {
    for name in [
        "fba_176k.mlp",
        "fba_192k.mlp",
        "fba_192k_8ch.mlp",
        "fba_2ch.mlp",
        "fba_atmos_cbi.mlp",
        "fba_atmos_dimtrim.mlp",
        "fba_atmos_obj.mlp",
        "fba_spliced.mlp",
        "fbb_6ch.mlp",
        "fbb_6ch_single.mlp",
        "fbb_copy.mlp",
        "fbb_spliced.mlp",
    ] {
        let data = asset(name);
        for selection in [-2, -1, 0, 1, 2, 3] {
            let d = Decoder::new(selection);
            d.push(&data, 0);
            let blocks = d.drain();
            assert!(!blocks.is_empty(), "{name} p{selection}: no output");
            let s = d.stats();
            assert_eq!(s.decode_errors + s.sync_errors, 0, "{name} p{selection}");
            let mut info = unsafe { std::mem::zeroed::<StreamInfo>() };
            assert_eq!(unsafe { truehd_atmos_decoder_get_stream_info(d.0, &mut info) }, OK);
            let p = blocks[0].0;
            assert_eq!(info.selected_presentation, p, "{name} p{selection}");
            assert_eq!(
                info.presentations[p as usize].channel_count, blocks[0].1,
                "{name} p{selection}: stream info channel count"
            );
            if selection == -2 {
                assert!(p <= 2, "{name}: channel-based selector decoded p{p}");
            }
        }
    }
}

/// Access-unit starts of a clean stream, by following the length fields.
fn access_units(data: &[u8]) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 8 <= data.len() {
        let len = ((u16::from_be_bytes([data[p], data[p + 1]]) & 0xFFF) as usize) * 2;
        if len < 8 {
            break;
        }
        let major = data[p + 4..p + 8] == [0xF8, 0x72, 0x6F, 0xBA];
        out.push((p, major));
        p += len;
    }
    out
}

fn first_block(d: &Decoder) -> Option<Block> {
    let mut b = empty_block();
    (unsafe { truehd_atmos_decoder_pull(d.0, &mut b) } == OK).then_some(b)
}

/// One-byte pushes from just after a major sync, where the next one is tens of kilobytes away:
/// input must never be refused for its chunking, and the count must come out exact.
#[test]
fn tiny_pushes_far_from_a_major_sync() {
    let Some(data) = sample("dolby-714-test-120s.thd").or_else(|| sample("dolby-unfold.thd")) else {
        return;
    };
    let aus = access_units(&data[..4 << 20]);
    let majors: Vec<usize> = (0..aus.len()).filter(|&i| aus[i].1).collect();
    // The access unit right after a major sync, where the next one is furthest away in bytes.
    let (start, next) = majors
        .windows(2)
        .map(|w| (w[0] + 1, w[1]))
        .max_by_key(|&(a, b)| aus[b].0 - aus[a].0)
        .unwrap();
    let d = Decoder::new(PRESENTATION_HIGHEST);
    let mut first = None;
    for (i, byte) in data[aus[start].0..].iter().enumerate() {
        assert_eq!(d.push(std::slice::from_ref(byte), i as i64), OK, "push {i} refused");
        if let Some(b) = first_block(&d) {
            first = Some(b);
            break;
        }
    }
    let b = first.expect("no block");
    assert_eq!(b.input_access_unit, (next - start) as u64);
    // pts is the index of the push holding the access unit's first byte while that push had a
    // record of its own; past 65536 waiting pushes it is unknown rather than wrong.
    let first_byte_push = (aus[next].0 - aus[start].0) as i64;
    if first_byte_push < 1 << 16 {
        assert_eq!((b.pts, b.pts_au_index), (first_byte_push, 0));
    } else {
        assert_eq!(b.pts, NO_PTS);
    }
    assert_eq!(b.flags & BLOCK_INPUT_OFFSET_ESTIMATED, 0);
    assert!(aus[next].0 - aus[start].0 > 1 << 16, "not far enough to exercise the limit");
}

/// A damaged first access unit after reset still counts, and is flagged as unverified.
#[test]
fn a_damaged_first_access_unit_is_counted() {
    let data = asset("fba_atmos_dimtrim.mlp");
    let aus = access_units(&data);
    assert!(aus[0].1, "fixture starts at a major sync");
    let next = (1..aus.len()).find(|&i| aus[i].1).unwrap();

    let mut damaged = data.clone();
    damaged[4 + 10] ^= 0x40; // inside the major sync info: its CRC no longer matches
    let d = Decoder::new(PRESENTATION_HIGHEST);
    assert_eq!(d.push(&damaged, 7), OK);
    let b = first_block(&d).expect("no block");
    assert_eq!(b.input_access_unit, next as u64, "access units before the next major sync");
    assert_eq!(b.pts, 7);
    assert_eq!(b.pts_au_index as u64, b.input_access_unit, "one push: pts index = ordinal");
    assert_ne!(b.flags & BLOCK_INPUT_OFFSET_ESTIMATED, 0);
    assert_eq!(d.stats().sync_errors, 1);
    let message = unsafe { std::ffi::CStr::from_ptr(truehd_atmos_decoder_last_error(d.0)) };
    assert!(!message.to_bytes().is_empty(), "damage must leave a message");
}

#[test]
fn version_string_names_the_commit_notice_names() {
    let version = unsafe { std::ffi::CStr::from_ptr(truehd_atmos_version_string()) }
        .to_str()
        .unwrap()
        .to_owned();
    let commit = version
        .split("truehdd@")
        .nth(1)
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("no truehdd commit in {version:?}"));
    assert_eq!(commit.len(), 12, "{version:?}");
    let notice = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/NOTICE")).unwrap();
    assert!(
        notice.split_whitespace().any(|word| word.trim_end_matches(&[':', ',', '.'][..]).len() == 40
            && word.starts_with(commit)),
        "NOTICE does not name the truehdd commit in {version:?}"
    );
}

/// Every plane and speaker code of every block: (main planes, main speakers, bed planes, bed speakers).
type Decoded = (Vec<Vec<f32>>, Vec<u8>, Vec<Vec<f32>>, Vec<u8>);

fn decode_all(d: &Decoder) -> Vec<Decoded> {
    let planes = |ptrs: *const *const f32, n: u32, frames: u32| -> Vec<Vec<f32>> {
        (0..n as usize)
            .map(|c| unsafe { std::slice::from_raw_parts(*ptrs.add(c), frames as usize).to_vec() })
            .collect()
    };
    let codes = |p: *const u8, n: u32| -> Vec<u8> {
        if n == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(p, n as usize).to_vec() }
        }
    };
    let mut out = Vec::new();
    loop {
        let mut b = empty_block();
        match unsafe { truehd_atmos_decoder_pull(d.0, &mut b) } {
            OK => out.push((
                planes(b.channels, b.channel_count, b.frame_count),
                codes(b.speakers, b.channel_count),
                planes(b.bed_channels, b.bed_channel_count, b.frame_count),
                codes(b.bed_speakers, b.bed_channel_count),
            )),
            NEED_MORE_DATA => return out,
            other => panic!("pull returned {other}"),
        }
    }
}

/// Pushes `data` in pieces small enough for the input limit, pulling everything after each.
fn feed(d: &Decoder, data: &[u8]) -> Vec<Decoded> {
    let mut out = Vec::new();
    for chunk in data.chunks(1 << 20) {
        assert_eq!(d.push(chunk, 0), OK);
        out.extend(decode_all(d));
    }
    out
}

fn atmos_fixtures() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = [
        "fba_atmos_cbi.mlp",
        "fba_atmos_dimtrim.mlp",
        "fba_atmos_obj.mlp",
    ]
    .iter()
    .map(|n| (n.to_string(), asset(n)))
    .collect();
    if let Some(data) = sample("dolby-unfold.thd") {
        v.push(("dolby-unfold.thd".into(), data));
    }
    v
}

/// The bed companion is the highest channel-based presentation, sample for sample, and leaves
/// the object presentation untouched.
#[test]
fn bed_companion_matches_a_channel_based_decode() {
    for (name, data) in atmos_fixtures() {
        let plain = feed(&Decoder::new(PRESENTATION_HIGHEST), &data);
        let bed = feed(&Decoder::new(PRESENTATION_HIGHEST_CHANNEL_BASED), &data);
        let companion = Decoder::new(PRESENTATION_HIGHEST);
        assert_eq!(
            unsafe { truehd_atmos_decoder_set_bed_companion(companion.0, true) },
            OK
        );
        let both = feed(&companion, &data);
        assert!(!both.is_empty(), "{name}: no output");
        assert_eq!(both.len(), plain.len(), "{name}: block count");
        assert_eq!(
            both.len(),
            bed.len(),
            "{name}: block count against the bed decode"
        );
        for (i, ((b, p), c)) in both.iter().zip(&plain).zip(&bed).enumerate() {
            assert!(p.2.is_empty(), "{name}: bed planes without the companion");
            assert_eq!(b.0, p.0, "{name} block {i}: object planes changed");
            assert_eq!(b.1, p.1, "{name} block {i}: object speakers changed");
            assert_eq!(b.2, c.0, "{name} block {i}: bed planes");
            assert_eq!(b.3, c.1, "{name} block {i}: bed speakers");
        }
    }
}

/// A channel-based selection has no companion to carry.
#[test]
fn bed_companion_is_empty_without_an_object_presentation() {
    let data = asset("fba_atmos_obj.mlp");
    let d = Decoder::new(PRESENTATION_HIGHEST_CHANNEL_BASED);
    assert_eq!(
        unsafe { truehd_atmos_decoder_set_bed_companion(d.0, true) },
        OK
    );
    d.push(&data, 0);
    let blocks = decode_all(&d);
    assert!(!blocks.is_empty());
    assert!(blocks.iter().all(|b| b.2.is_empty() && b.3.is_empty()));
}

/// Discarding interpolation changes an interpolating object presentation (in a build with the
/// `matrix-interpolation` feature), survives reset(), and is per handle.
#[test]
fn ignore_interpolation_is_per_handle_and_survives_reset() {
    let Some(data) = sample("dolby-unfold.thd") else {
        eprintln!("skipped: dolby-unfold.thd not in the work directory");
        return;
    };
    let exact = Decoder::new(PRESENTATION_HIGHEST);
    let held = Decoder::new(PRESENTATION_HIGHEST);
    assert_eq!(
        unsafe { truehd_atmos_decoder_set_ignore_interpolation(held.0, true) },
        OK
    );
    let a = feed(&exact, &data);
    let b = feed(&held, &data);
    assert!(!a.is_empty());
    assert_eq!(a.len(), b.len());
    if cfg!(feature = "matrix-interpolation") {
        assert!(a.iter().zip(&b).any(|(x, y)| x.0 != y.0), "no block changed");
    } else {
        // Without the feature no handle interpolates.
        assert!(a.iter().zip(&b).all(|(x, y)| x.0 == y.0));
    }
    assert_eq!(unsafe { truehd_atmos_decoder_reset(held.0) }, OK);
    assert_eq!(
        feed(&held, &data).iter().map(|x| &x.0).collect::<Vec<_>>(),
        b.iter().map(|x| &x.0).collect::<Vec<_>>()
    );
}

/// (output_timing, output_position or None, input_timing, frame_count) of every block.
fn clocks(d: &Decoder) -> Vec<(u32, Option<u32>, u32, u32)> {
    let mut out = Vec::new();
    loop {
        let mut b = empty_block();
        match unsafe { truehd_atmos_decoder_pull(d.0, &mut b) } {
            OK => out.push((
                b.output_timing,
                (b.flags & BLOCK_OUTPUT_POSITION != 0).then_some(b.output_position),
                b.input_timing,
                b.frame_count,
            )),
            NEED_MORE_DATA => return out,
            other => panic!("pull returned {other}"),
        }
    }
}

fn feed_clocks(d: &Decoder, data: &[u8]) -> Vec<(u32, Option<u32>, u32, u32)> {
    let mut out = Vec::new();
    for chunk in data.chunks(1 << 20) {
        assert_eq!(d.push(chunk, 0), OK);
        out.extend(clocks(d));
    }
    out
}

/// output_timing is the stream's presentation clock: block after block it advances by the
/// frame count, restarting only where the stream is spliced, and a decode started at any
/// access unit reads the same clock for the same audio. Where the stream sends
/// hires_output_timing the 32-bit position agrees with it.
#[test]
fn output_timing_is_the_streams_presentation_clock() {
    let mut streams: Vec<(String, Vec<u8>)> = [
        "fba_176k.mlp",
        "fba_192k.mlp",
        "fba_192k_8ch.mlp",
        "fba_2ch.mlp",
        "fba_atmos_cbi.mlp",
        "fba_atmos_dimtrim.mlp",
        "fba_atmos_obj.mlp",
        "fba_spliced.mlp",
    ]
    .iter()
    .map(|n| (n.to_string(), asset(n)))
    .collect();
    for name in ["helicopter.thd", "atmos-714.thd"] {
        if let Some(data) = sample(name) {
            streams.push((name.into(), data));
        }
    }
    for (name, data) in streams {
        let d = Decoder::new(PRESENTATION_HIGHEST);
        let full = feed_clocks(&d, &data);
        assert!(full.len() > 2, "{name}");
        let breaks = full
            .windows(2)
            .filter(|w| (w[1].0.wrapping_sub(w[0].0) & 0xFFFF) != w[0].3)
            .count();
        if name.contains("spliced") {
            assert!((1..=2).contains(&breaks), "{name}: {breaks} breaks");
        } else {
            assert_eq!(breaks, 0, "{name}: output_timing did not advance by the frame count");
        }
        for (output_timing, position, ..) in &full {
            if let Some(position) = position {
                assert_eq!(position & 0xFFFF, *output_timing, "{name}: position and output_timing disagree");
            }
        }
        if name.ends_with(".thd") {
            let positioned = full.iter().filter(|c| c.1.is_some()).count();
            assert!(positioned * 10 > full.len() * 9, "{name}: {positioned} of {} blocks have a position", full.len());
        }

        // From the access unit after the first major sync, so decoding starts at the second:
        // the same clocks, block for block.
        let aus = access_units(&data);
        if aus.iter().filter(|a| a.1).count() < 2 {
            continue;
        }
        let start = aus.iter().position(|a| a.1).unwrap() + 1;
        let d = Decoder::new(PRESENTATION_HIGHEST);
        let tail = feed_clocks(&d, &data[aus[start].0..]);
        assert!(!tail.is_empty(), "{name}: nothing after the seek");
        let at = full
            .iter()
            .position(|c| c.0 == tail[0].0 && c.2 == tail[0].2)
            .unwrap_or_else(|| panic!("{name}: the seek's clock is not in the full decode"));
        let pair = |c: &(u32, Option<u32>, u32, u32)| (c.0, c.2, c.3);
        assert_eq!(
            full[at..at + tail.len()].iter().map(pair).collect::<Vec<_>>(),
            tail.iter().map(pair).collect::<Vec<_>>(),
            "{name}: the seek reads a different clock"
        );
        for (a, b) in full[at..].iter().zip(&tail) {
            if let (Some(x), Some(y)) = (a.1, b.1) {
                assert_eq!(x, y, "{name}: the seek's position differs");
            }
        }
    }
}

/// A damaged access unit header whose parity still passes is caught by its input_timing:
/// XOR-ing 0x11 into both timing bytes keeps the header parity and moves the timing out of
/// the bounds the format sets.
#[test]
fn a_header_whose_parity_passes_is_still_caught_by_its_timing() {
    let data = asset("fba_atmos_dimtrim.mlp");
    let aus = access_units(&data);
    let clean = Decoder::new(PRESENTATION_HIGHEST);
    let blocks = feed_clocks(&clean, &data).len();
    assert_eq!(clean.stats().sync_errors, 0);

    // A non-major access unit in the middle whose altered timing leaves the bounds.
    let (i, _) = aus
        .iter()
        .enumerate()
        .skip(aus.len() / 2)
        .find(|&(i, a)| {
            if a.1 || aus[i - 1].1 {
                return false;
            }
            let prev = u16::from_be_bytes([data[aus[i - 1].0 + 2], data[aus[i - 1].0 + 3]]);
            let t = u16::from_be_bytes([data[a.0 + 2], data[a.0 + 3]]) ^ 0x1111;
            let interval = t.wrapping_sub(prev);
            !(10..=3600).contains(&interval)
        })
        .expect("no access unit to alter");
    let mut damaged = data.clone();
    damaged[aus[i].0 + 2] ^= 0x11;
    damaged[aus[i].0 + 3] ^= 0x11;
    let d = Decoder::new(PRESENTATION_HIGHEST);
    let out = feed_clocks(&d, &damaged);
    assert!(d.stats().sync_errors >= 1, "the altered header was accepted");
    assert!(out.len() < blocks, "nothing was dropped to resync");
    // After the resync the clock is the stream's again.
    let last = out.last().unwrap();
    let full = feed_clocks(&Decoder::new(PRESENTATION_HIGHEST), &data);
    assert_eq!(last.0, full.last().unwrap().0);
}
