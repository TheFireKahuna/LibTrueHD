//! Every dynamic object's coded position and ramp, one line per object and metadata block, from a
//! TrueHD stream on stdin: `sample object x y z ramp`. For studying how objects move between updates.
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
    let mut samples = 0u64;
    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        extractor.push_bytes(&buf[..n]);
        while let Some(Ok(frame)) = extractor.next() {
            let Ok(au) = parser.parse(&frame) else { continue };
            let Ok(decoded) = decoder.decode_presentations(&au, &[false, false, false, true]) else { continue };
            let Some(d) = decoded[3].as_ref() else { continue };
            for p in &d.oamd {
                let Some(oe) = &p.object_element else { continue };
                let info = &oe.md_update_info;
                for (o, blocks) in oe.object_data.iter().enumerate() {
                    for (b, block) in blocks.iter().enumerate() {
                        if block.b_object_in_bed_or_isf || block.b_object_not_active {
                            continue;
                        }
                        let u = &info.block_update_info[b];
                        let at = samples + p.evo_sample_offset + info.sample_offset as u64 + 32 * u.block_offset_factor_bits as u64;
                        let [x, y, z] = block.object_render_info.pos3d;
                        println!("{at}\t{o}\t{x:.4}\t{y:.4}\t{z:.4}\t{}", u.ramp_duration);
                    }
                }
            }
            samples += d.sample_length as u64;
        }
    }
    Ok(())
}
