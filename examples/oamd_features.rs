//! Which TS 103 420 features a stream's object metadata uses: size, zones, snap, divergence,
//! distance, screen reference, elevation, extended precision, trims and warp. Reads the TrueHD stream from stdin as it
//! arrives, so a whole film can be piped in without storing it.
//!
//! usage: ffmpeg -i <film> -map 0:a:<n> -c copy -f truehd - | cargo run --release --example oamd_features
use std::collections::BTreeMap;
use std::io::Read;
use truehd::process::decode::Decoder;
use truehd::process::extract::Extractor;
use truehd::process::parse::Parser;

fn main() -> anyhow::Result<()> {
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    parser.set_check_fifo(false);
    parser.set_required_presentations(&[false, false, false, true]);
    let mut decoder = Decoder::default();

    let mut count: BTreeMap<&str, u64> = BTreeMap::new();
    let mut objects: BTreeMap<usize, u64> = BTreeMap::new();
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    let mut zones: BTreeMap<u8, u64> = BTreeMap::new();
    let mut warp: BTreeMap<u8, u64> = BTreeMap::new();
    let mut trim_mode: BTreeMap<u8, u64> = BTreeMap::new();
    let mut first: BTreeMap<&str, u64> = BTreeMap::new();
    let mut samples = 0u64;
    let mut note = |count: &mut BTreeMap<&'static str, u64>, what: &'static str, at: u64| {
        *count.entry(what).or_default() += 1;
        first.entry(what).or_insert(at);
    };

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
                Some(Err(e)) => {
                    eprintln!("extract: {e}");
                    continue;
                }
            };
            // A payload the parser rejects fails its access unit, so failures are counted, not hidden.
            let Ok(access_unit) = parser.parse(&frame) else {
                *count.entry("parse errors").or_default() += 1;
                continue;
            };
            let Ok(decoded) = decoder.decode_presentations(&access_unit, &[false, false, false, true]) else {
                *count.entry("decode errors").or_default() += 1;
                continue;
            };
            let Some(d) = decoded[3].as_ref() else { continue };
            for p in &d.oamd {
                note(&mut count, "payloads", samples);
                *objects.entry(p.object_count).or_default() += 1;
                if let Some(t) = &p.trim_element {
                    *warp.entry(t.warp_mode).or_default() += 1;
                    *trim_mode.entry(t.global_trim_mode).or_default() += 1;
                }
                if let Some(e) = &p.extended_object_element {
                    if e.b_ext_prec_pos_block {
                        note(&mut count, "extended precision", samples);
                    }
                    if e.object_div_block.iter().flatten().any(|b| b.object_divergence > 0.0) {
                        note(&mut count, "divergence", samples);
                    }
                }
                let Some(oe) = &p.object_element else { continue };
                for block in oe.object_data.iter().flatten() {
                    if block.b_object_in_bed_or_isf || block.b_object_not_active {
                        continue;
                    }
                    let r = &block.object_render_info;
                    let size = r.object_size[0].max(r.object_size[1]).max(r.object_size[2]);
                    if size > 0.0 {
                        note(&mut count, "size", samples);
                        *sizes.entry(format!("{size:.2}")).or_default() += 1;
                    }
                    if r.zone_constraints_idx != 0 {
                        note(&mut count, "zones", samples);
                    }
                    *zones.entry(r.zone_constraints_idx).or_default() += 1;
                    if r.b_object_snap {
                        note(&mut count, "snap", samples);
                    }
                    if !r.b_enable_elevation {
                        note(&mut count, "elevation off", samples);
                    }
                    if r.b_object_use_screen_ref {
                        note(&mut count, "screen", samples);
                    }
                    if r.b_object_distance_specified {
                        note(&mut count, "distance", samples);
                    }
                }
            }
            samples += d.sample_length as u64;
        }
    }
    println!("seconds {:.0}", samples as f64 / 48000.0);
    println!("counts {count:?}");
    println!("first seen (s) {:?}", first.iter().map(|(k, v)| (*k, v / 48000)).collect::<BTreeMap<_, _>>());
    println!("object_count {objects:?}");
    println!("sizes {sizes:?}");
    println!("zones {zones:?}");
    println!("warp {warp:?} trim_mode {trim_mode:?}");
    Ok(())
}
