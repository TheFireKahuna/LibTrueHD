//! What a stream carries in the OAMD elements TS 103 420 leaves reserved (headphone, 3; object
//! description, 4; bed object, 6) and in the trim element: how often each comes, every value it
//! takes, per bed channel and per object. Parses only (no audio decode), from stdin, so windows of
//! many films can be piped through quickly.
//!
//! usage: ffmpeg -ss <t> -i <film> -t <s> -map 0:<i> -c copy -f truehd - | oamd_elements [label]
use std::collections::BTreeMap;
use std::io::Read;
use truehd::process::extract::Extractor;
use truehd::process::parse::Parser;
use truehd::structs::oamd::ObjectAudioMetadataPayload;

const OAMD_PAYLOAD_ID: u32 = 11;

#[derive(Default)]
struct Tally(BTreeMap<String, u64>);

impl Tally {
    fn add(&mut self, key: impl Into<String>) {
        *self.0.entry(key.into()).or_default() += 1;
    }
}

fn main() -> anyhow::Result<()> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "stdin".into());
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    parser.set_check_fifo(false);
    parser.set_required_presentations(&[false, false, false, true]);

    let mut t = Tally::default();
    // Per element index: the last dialog indication, to count how often it changes.
    let mut last_dialog: BTreeMap<usize, u8> = BTreeMap::new();
    let mut access_units = 0u64;

    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        extractor.push_bytes(&buf[..n]);
        loop {
            let frame = match extractor.next() {
                Some(Ok(f)) => f,
                Some(Err(truehd::utils::errors::ExtractError::InsufficientData)) | None => break,
                Some(Err(_)) => continue,
            };
            let Ok(au) = parser.parse(&frame) else {
                t.add("parse error");
                continue;
            };
            access_units += 1;
            let Some(evo) = au.extra_data.as_ref().and_then(|x| x.evo_frame.as_ref()) else { continue };
            for payload in &evo.evo_payloads {
                t.add(format!("evo payload id {}", payload.evo_payload_id));
                if payload.evo_payload_id != OAMD_PAYLOAD_ID {
                    continue;
                }
                let Ok(p) = ObjectAudioMetadataPayload::read(&payload.evo_payload_byte) else {
                    t.add("oamd error");
                    continue;
                };
                survey(&p, &mut t, &mut last_dialog);
            }
        }
    }
    println!("== {label}: {access_units} access units");
    for (k, v) in &t.0 {
        println!("{v:>9}  {k}");
    }
    Ok(())
}

fn survey(p: &ObjectAudioMetadataPayload, t: &mut Tally, last_dialog: &mut BTreeMap<usize, u8>) {
    t.add("oamd payloads");
    for md in &p.oa_element_md {
        t.add(format!("element id {}", md.oa_element_id_idx));
        if let Some(alt) = md.alternate_object_data_id_idx {
            t.add(format!("alternate data id {alt}"));
        }
    }
    let prog = &p.program_assignment;
    let beds = prog.num_bed_objects;
    let labels: Vec<usize> = prog.bed_assignment.iter().flat_map(|b| b.to_index_vec()).collect();
    t.add(format!("config: {} elements, {beds} bed {labels:?}, {} isf, {} dynamic",
                  p.object_count, prog.num_isf_objects, prog.num_dynamic_objects));
    let who = |i: usize| -> String {
        if i < beds { format!("bed label {}", labels.get(i).copied().unwrap_or(99)) } else { "object".into() }
    };

    if let Some(d) = &p.object_description_element {
        t.add(format!("objdesc present={} sub_mode={} blocks={}", d.b_present, d.sub_mode, d.num_obj_info_blocks));
        for (i, blocks) in d.object_dialog_indication.iter().enumerate() {
            for &v in blocks {
                t.add(format!("objdesc dialog={v} on {}", who(i)));
                t.add(format!("objdesc dialog={v} on element {i:02}"));
                if last_dialog.insert(i, v).is_some_and(|old| old != v) {
                    t.add(format!("objdesc dialog changes on element {i:02}"));
                }
            }
        }
    }
    if let Some(h) = &p.headphone_element {
        t.add(format!("headphone hp_mode={} blocks={}", h.hp_mode, h.num_obj_info_blocks));
        for (i, blocks) in h.headphone_data.iter().enumerate() {
            for b in blocks {
                t.add(format!("headphone render_mode={} head_track_disable={} on {}",
                              b.hp_render_mode, b.hp_head_track_disable, who(i)));
            }
        }
    }
    if let Some(x) = &p.extended_object_element {
        t.add(format!("extended: div_block={} ext_prec_block={}", x.b_obj_div_block, x.b_ext_prec_pos_block));
        let divergent = x.object_div_block.iter().flatten().filter(|d| d.b_object_divergence).count();
        if divergent > 0 {
            t.add(format!("extended: {divergent} divergent object blocks"));
        }
    }
    if let Some(b) = &p.bed_object_element {
        t.add(format!("bed object element: {b:?}"));
    }
    if let Some(trim) = &p.trim_element {
        t.add(format!("trim warp={} global={} per_obj={}", trim.warp_mode, trim.global_trim_mode,
                      trim.b_disable_trim_per_obj));
        for (cfg, c) in trim.trims.iter().enumerate() {
            if let Some(c) = c {
                t.add(format!("trim cfg {cfg}: {c:?}"));
            }
        }
    }
}
