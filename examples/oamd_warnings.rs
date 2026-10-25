//! Counts the warnings the parsers log over a stream (the OAMD parser's included), by message with
//! its numbers stripped: an element that overruns its declared size, an unreadable Evolution frame.
use std::collections::BTreeMap;
use std::sync::Mutex;
use truehd::process::decode::Decoder;
use truehd::process::extract::Extractor;
use truehd::process::parse::Parser;

static WARNINGS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

struct Counter;
impl log::Log for Counter {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if r.level() <= log::Level::Warn {
            let text: String = format!("{}: {}", r.target(), r.args()).chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
            *WARNINGS.lock().unwrap().entry(text).or_default() += 1;
        }
    }
    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    log::set_logger(&Counter).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    let path = std::env::args().nth(1).expect("path to .thd");
    let data = std::fs::read(&path)?;
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    parser.set_check_fifo(false);
    parser.set_required_presentations(&[false, false, false, true]);
    let mut decoder = Decoder::default();
    let (mut payloads, mut objects, mut blocks) = (0u64, 0u64, 0u64);
    for chunk in data.chunks(4096) {
        extractor.push_bytes(chunk);
        loop {
            let frame = match extractor.next() {
                Some(Ok(f)) => f,
                Some(Err(truehd::utils::errors::ExtractError::InsufficientData)) | None => break,
                Some(Err(_)) => continue,
            };
            let access_unit = parser.parse(&frame)?;
            let decoded = decoder.decode_presentations(&access_unit, &[false, false, false, true])?;
            let Some(d) = decoded[3].as_ref() else { continue };
            for p in &d.oamd {
                payloads += 1;
                if p.object_element.is_some() {
                    objects += 1;
                }
                if let Some(oe) = &p.object_element {
                    blocks += oe.object_data.iter().map(Vec::len).sum::<usize>() as u64;
                }
            }
        }
    }
    println!("{path}: {payloads} payloads, {objects} with an object element, {blocks} object blocks");
    for (w, n) in WARNINGS.lock().unwrap().iter() {
        println!("  {n:>8}  {w}");
    }
    Ok(())
}
