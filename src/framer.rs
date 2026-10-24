//! Access-unit framing with an exact count of every access unit pushed.
//!
//! This replaces `truehd::process::extract::Extractor` for one reason the extractor cannot
//! serve: after a seek it discards whatever precedes the first major sync as raw bytes, so
//! nothing says how many access units those bytes held, and the caller could not place the
//! first decoded block on its timeline.
//!
//! Here, until a major sync is found (after create, reset, or damage) the bytes are kept.
//! Once one is found, the access units before it are recovered by chaining backwards: the
//! earliest position whose length fields lead, hop by hop and with valid header parity,
//! exactly onto the major sync starts the chain, and every access unit on it is reported (as
//! skipped) with its own ordinal. Only a leading fragment shorter than an access unit, the
//! tail of one that began before the first byte pushed, is left uncounted.
//!
//! Validation mirrors the extractor: a major sync counts only with a correct major sync info
//! CRC and header parity, and every other access unit's header nibble parity is checked
//! against the substream count the last major sync declared. The header's input_timing is
//! checked too, as the format bounds it: from one access unit to the next it advances by at
//! least a quarter of an access unit, at most 75 ms, and never faster than the declared peak
//! data rate allows. A major sync is exempt (a splice or seamless branch restarts the clock
//! there, and its CRC already proves it). Parity alone passes one damaged header in 16; with
//! the timing bounds about one in three hundred.

use truehd::structs::sync::{MAJOR_SYNC_FBA, MAJOR_SYNC_FBB};
use truehd::utils::crc::{CRC_MAJOR_SYNC_INFO_ALG, Crc16};

/// Smallest access unit accepted, and the bytes needed to tell a major sync from a minor one.
const MIN_AU_LEN: usize = 8;
/// Largest access unit the 12-bit length field can state.
const MAX_AU_LEN: usize = 0xFFF * 2;
/// Bytes kept from the scan position when no sync word is buffered: an access-unit header
/// and three bytes of a sync word.
const SCAN_KEEP: usize = 4 + 3;
/// Bytes retained while seeking a major sync before the oldest are let go (and the access
/// units in them estimated). FBA places a major sync at least every 128 access units.
const MAX_RETAINED: usize = 4 << 20;

/// What the last major sync says an access unit's input_timing may do.
#[derive(Clone, Copy, Debug)]
struct TimingBounds {
    min_interval: u32,
    max_interval: u32,
    /// Some(peak_data_rate) for a variable-rate stream.
    peak_data_rate: Option<u32>,
}

impl TimingBounds {
    /// From a major sync access unit's bytes (validated).
    fn of_major_sync(b: &[u8]) -> Self {
        let fbb = be32(&b[4..8]) == MAJOR_SYNC_FBB;
        let code = if fbb { b[9] >> 4 } else { b[8] >> 4 };
        let rate = match code {
            0..=2 => 48_000u32 << code,
            8..=10 => 44_100u32 << (code - 8),
            _ => 48_000,
        };
        let samples_per_au = 40 * (rate / 44_100);
        let rate_word = u16::from_be_bytes([b[18], b[19]]);
        Self {
            min_interval: samples_per_au / 4,
            max_interval: (rate * 3).div_ceil(40),
            peak_data_rate: (rate_word & 0x8000 != 0).then_some((rate_word & 0x7FFF) as u32),
        }
    }

    /// Whether an access unit with `timing` may follow one with `prev_timing` that was
    /// `prev_words` 16-bit words long.
    fn admits(&self, prev_timing: u16, prev_words: u32, timing: u16) -> bool {
        let interval = timing.wrapping_sub(prev_timing) as u32;
        interval >= self.min_interval
            && interval <= self.max_interval
            && self
                .peak_data_rate
                .is_none_or(|peak| (prev_words as u64) << 8 <= interval as u64 * peak as u64)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AuInfo {
    /// Byte offset of the access unit's first byte, counted from the first byte pushed.
    pub offset: u64,

    /// Access units before this one since the framer was created or reset.
    pub ordinal: u64,
    pub major_sync: bool,
    /// The header's input_timing: when the access unit's first byte enters the decoder's
    /// FIFO in the format's buffer model, modulo 2^16. Not the access unit's presentation
    /// time (that is output_timing); see the header. Zero for one only counted.
    pub input_timing: u16,
}

pub enum Next<'a> {
    /// A decodable access unit and its bytes.
    Au(AuInfo, &'a [u8]),
    /// An access unit before the first major sync after a seek or damage: counted, but it
    /// cannot be decoded.
    Skipped(AuInfo),
    NeedData,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Locked,
    /// Looking for a major sync. Bytes from the cursor on are kept; `scan` is the buffer index
    /// the search continues from. `after_damage`: the cursor is the start of an access unit
    /// that failed its checks, rather than wherever the caller's first push began.
    Seeking { scan: usize, after_damage: bool },
}

pub struct Framer {
    buf: Vec<u8>,
    cursor: usize,
    /// Stream offset of `buf[cursor]`.
    consumed: u64,
    state: State,
    /// Lengths of the access units recovered by chaining back from a major sync, to report.
    queued: Vec<usize>,
    queued_next: usize,
    /// Substream count declared by the last accepted major sync.
    substreams: usize,
    /// input_timing bounds from the last accepted major sync.
    bounds: Option<TimingBounds>,
    /// input_timing and length in words of the last access unit handed out, while locked.
    last_timing: Option<(u16, u32)>,
    ordinal: u64,
    estimated: bool,
    /// Access units and their bytes framed since creation, for estimates.
    framed_aus: u64,
    framed_bytes: u64,
    crc: Crc16,
    pub sync_errors: u64,
    pub skipped_bytes: u64,
}

impl Default for Framer {
    fn default() -> Self {
        Self {
            buf: Vec::with_capacity(128 * 1024),
            cursor: 0,
            consumed: 0,
            state: State::Seeking {
                scan: 0,
                after_damage: false,
            },
            queued: Vec::with_capacity(160),
            queued_next: 0,
            substreams: 0,
            bounds: None,
            last_timing: None,
            ordinal: 0,
            estimated: false,
            framed_aus: 0,
            framed_bytes: 0,
            crc: Crc16::new(&CRC_MAJOR_SYNC_INFO_ALG),
            sync_errors: 0,
            skipped_bytes: 0,
        }
    }
}

impl Framer {
    /// Drops all buffered bytes and restarts counting; the next byte pushed is offset 0.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.consumed = 0;
        self.state = State::Seeking {
            scan: 0,
            after_damage: false,
        };
        self.queued.clear();
        self.queued_next = 0;
        self.last_timing = None;
        self.ordinal = 0;
        self.estimated = false;
        self.sync_errors = 0;
        self.skipped_bytes = 0;
    }

    pub fn push(&mut self, data: &[u8]) {
        self.compact(data.len());
        self.buf.extend_from_slice(data);
    }

    /// Bytes pushed but not yet consumed.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.cursor
    }

    /// Whether any ordinal handed out since reset rests on an estimate.
    pub fn estimated(&self) -> bool {
        self.estimated
    }

    fn compact(&mut self, incoming: usize) {
        if self.cursor == 0 {
            return;
        }
        let needs_room = self.buf.len() + incoming > self.buf.capacity();
        if self.cursor == self.buf.len()
            || needs_room
            || self.cursor >= 64 * 1024
            || self.cursor * 2 >= self.buf.len()
        {
            self.buf.drain(..self.cursor);
            if let State::Seeking { scan, .. } = &mut self.state {
                *scan -= self.cursor;
            }
            self.cursor = 0;
        }
    }

    fn consume(&mut self, n: usize) {
        self.cursor += n;
        self.consumed += n as u64;
    }

    fn take_ordinal(&mut self, len: usize) -> u64 {
        let ordinal = self.ordinal;
        self.ordinal += 1;
        self.framed_aus += 1;
        self.framed_bytes += len as u64;
        ordinal
    }

    fn average_au_len(&self) -> f64 {
        if self.framed_aus == 0 {
            1024.0
        } else {
            self.framed_bytes as f64 / self.framed_aus as f64
        }
    }

    pub fn next(&mut self) -> Next<'_> {
        loop {
            if self.queued_next < self.queued.len() {
                let len = self.queued[self.queued_next];
                self.queued_next += 1;
                let offset = self.consumed;
                let input_timing = u16::from_be_bytes([self.buf[self.cursor + 2], self.buf[self.cursor + 3]]);
                self.last_timing = Some((input_timing, (len / 2) as u32));
                let ordinal = self.take_ordinal(len);
                self.consume(len);
                return Next::Skipped(AuInfo {
                    offset,
                    ordinal,
                    major_sync: false,
                    input_timing,
                });
            }
            match self.state {
                State::Seeking {
                    scan,
                    after_damage,
                } => {
                    if !self.seek(scan, after_damage) {
                        return Next::NeedData;
                    }
                }
                State::Locked => match self.check_at(self.cursor) {
                    Check::Valid { len, major } => {
                        let start = self.cursor;
                        let offset = self.consumed;
                        let input_timing = u16::from_be_bytes([self.buf[start + 2], self.buf[start + 3]]);
                        self.last_timing = Some((input_timing, (len / 2) as u32));
                        let ordinal = self.take_ordinal(len);
                        self.consume(len);
                        return Next::Au(
                            AuInfo {
                                offset,
                                ordinal,
                                major_sync: major,
                                input_timing,
                            },
                            &self.buf[start..start + len],
                        );
                    }
                    Check::NeedData => return Next::NeedData,
                    Check::Invalid => {
                        self.sync_errors += 1;
                        self.last_timing = None;
                        self.state = State::Seeking {
                            scan: self.cursor + 1,
                            after_damage: true,
                        };
                    }
                },
            }
        }
    }

    /// Checks the access unit at `at` in the locked chain. A valid major sync updates the
    /// substream count the following access units are checked against.
    fn check_at(&mut self, at: usize) -> Check {
        let b = &self.buf[at..];
        if b.len() < MIN_AU_LEN {
            return Check::NeedData;
        }
        let len = au_len(b);
        if len < MIN_AU_LEN {
            return Check::Invalid;
        }
        if is_sync_word(be32(&b[4..8])) {
            match self.major_sync_at(at) {
                Some(Some(substreams)) => {
                    self.substreams = substreams;
                    self.bounds = Some(TimingBounds::of_major_sync(&self.buf[at..]));
                    Check::Valid { len, major: true }
                }
                Some(None) => Check::Invalid,
                None => Check::NeedData,
            }
        } else {
            let timing = u16::from_be_bytes([b[2], b[3]]);
            let timed = match (self.bounds, self.last_timing) {
                (Some(bounds), Some((prev, prev_words))) => bounds.admits(prev, prev_words, timing),
                _ => true,
            };
            match parity(b, self.substreams, 0) {
                Some(true) if !timed => Check::Invalid,
                Some(true) if b.len() >= len => Check::Valid { len, major: false },
                Some(true) => Check::NeedData,
                Some(false) => Check::Invalid,
                None if b.len() >= len => Check::Invalid,
                None => Check::NeedData,
            }
        }
    }

    /// Validates a major sync access unit starting at `at`: `None` if more bytes are needed,
    /// `Some(None)` if it is not a valid one, else its substream count.
    fn major_sync_at(&self, at: usize) -> Option<Option<usize>> {
        let b = &self.buf[at..];
        let msi = major_sync_info_len(b)?;
        let len = au_len(b);
        if len <= msi + 6 {
            return Some(None);
        }
        if b.len() < len {
            return None;
        }
        let crc = u16::from_be_bytes([b[4 + msi], b[5 + msi]]);
        if crc != self.crc.update(self.crc.init, &b[4..4 + msi]) {
            return Some(None);
        }
        let substreams = (b[20] >> 4) as usize;
        Some((parity(&b[..len], substreams, msi + 2) == Some(true)).then_some(substreams))
    }

    /// Searches for a major sync from buffer index `scan`, keeping every byte from the cursor.
    /// Returns true once locked (with the access units before the sync queued).
    fn seek(&mut self, mut scan: usize, after_damage: bool) -> bool {
        loop {
            // A candidate access unit start `c` has its sync word at c + 4.
            let found = self.buf[scan..]
                .windows(8)
                .position(|w| is_sync_word(be32(&w[4..8])))
                .map(|p| scan + p);
            let Some(c) = found else {
                let resume = self.buf.len().saturating_sub(SCAN_KEEP).max(scan);
                // Let the oldest bytes go if nothing has turned up for this long.
                if resume - self.cursor > MAX_RETAINED {
                    let drop = resume - self.cursor;
                    self.ordinal += (drop as f64 / self.average_au_len()).round() as u64;
                    self.estimated = true;
                    self.skipped_bytes += drop as u64;
                    self.consume(drop);
                }
                self.state = State::Seeking {
                    scan: resume,
                    after_damage,
                };
                return false;
            };
            match self.major_sync_at(c) {
                None => {
                    self.state = State::Seeking {
                        scan: c,
                        after_damage,
                    };
                    return false;
                }
                Some(None) => scan = c + 1,
                Some(Some(substreams)) => {
                    self.substreams = substreams;
                    self.bounds = Some(TimingBounds::of_major_sync(&self.buf[c..]));
                    self.chain_back(c, after_damage);
                    self.state = State::Locked;
                    return true;
                }
            }
        }
    }

    /// Recovers the access units between the cursor and the major sync at buffer index `sync`,
    /// queues them, and consumes the fragment in front of them.
    fn chain_back(&mut self, sync: usize, after_damage: bool) {
        let region = &self.buf[self.cursor..sync];
        let n = region.len();
        self.queued.clear();
        self.queued_next = 0;

        // hops[(n - p) / 2]: access units on the chain from position p to the sync, 0 if p
        // does not start one. Lengths are whole 16-bit words, so every start on a chain into
        // the sync is an even distance before it. Each hop between two of them must also keep
        // input_timing within the bounds; the hop into the major sync is exempt, as when locked.
        let bounds = self.bounds;
        let mut first = n;
        if n >= MIN_AU_LEN {
            let mut hops = vec![0u32; n / 2 + 1];
            let mut p = n - 2;
            loop {
                let a = &region[p..];
                if a.len() >= MIN_AU_LEN {
                    let len = au_len(a);
                    let starts_chain = len >= MIN_AU_LEN
                        && len <= a.len()
                        && !is_sync_word(be32(&a[4..8]))
                        && parity(&a[..len], self.substreams, 0) == Some(true);
                    let next = p + len;
                    // Only a hop onto a chain start is checked: the bytes there are an access unit.
                    let timed = || {
                        next == n
                            || bounds.is_none_or(|bounds| {
                                bounds.admits(
                                    u16::from_be_bytes([a[2], a[3]]),
                                    (len / 2) as u32,
                                    u16::from_be_bytes([region[next + 2], region[next + 3]]),
                                )
                            })
                    };
                    if starts_chain && (next == n || hops[(n - next) / 2] > 0) && timed() {
                        let h = if next == n { 1 } else { hops[(n - next) / 2] + 1 };
                        if h > 1 || next == n {
                            hops[(n - p) / 2] = h;
                            first = p;
                        }
                    }
                }
                if p < 2 {
                    break;
                }
                p -= 2;
            }
            let mut p = first;
            while p < n {
                let len = au_len(&region[p..]);
                self.queued.push(len);
                p += len;
            }
        }

        // The bytes in front of the chain. If their own length fields lead exactly onto the
        // chain, they are whole access units that failed their checks (damage, or a damaged
        // first access unit after a reset): report each one, so its ordinal and the caller's
        // pts attribution stay right, but flag the count as unverified. Otherwise, after damage
        // (or when too long to be a fragment) estimate them; else they are the tail of an
        // access unit that began before the first byte pushed and are not counted.
        let fragment = first;
        if fragment > 0 {
            let region = &self.buf[self.cursor..self.cursor + fragment];
            let mut whole = Vec::new();
            let mut p = 0;
            while p + MIN_AU_LEN <= fragment {
                let len = au_len(&region[p..]);
                if len < MIN_AU_LEN || p + len > fragment {
                    break;
                }
                whole.push(len);
                p += len;
            }
            if p == fragment {
                if !after_damage {
                    self.sync_errors += 1;
                }
                self.estimated = true;
                whole.extend_from_slice(&self.queued);
                self.queued = whole;
            } else {
                if after_damage || fragment > MAX_AU_LEN {
                    let estimate =
                        (fragment as f64 / self.average_au_len()).round().max(1.0) as u64;
                    self.ordinal += estimate;
                    self.estimated = true;
                }
                self.skipped_bytes += fragment as u64;
                self.consume(fragment);
            }
        }
    }
}

enum Check {
    Valid { len: usize, major: bool },
    NeedData,
    Invalid,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn is_sync_word(w: u32) -> bool {
    w == MAJOR_SYNC_FBA || w == MAJOR_SYNC_FBB
}

fn au_len(b: &[u8]) -> usize {
    ((u16::from_be_bytes([b[0], b[1]]) & 0x0FFF) as usize) << 1
}

/// Length of the major sync info block that starts four bytes into the access unit, as the
/// extractor derives it: FBB is always 26 bytes, FBA 26 or longer with extra channel meaning.
fn major_sync_info_len(b: &[u8]) -> Option<usize> {
    if *b.get(7)? == 0xBB {
        return Some(26);
    }
    Some(if b.get(29)? & 1 == 0 {
        26
    } else {
        28 + ((b.get(30)? >> 3) & 0x1E) as usize
    })
}

/// Access-unit header nibble parity over the 4-byte header and the substream directory, with
/// `skip` bytes (the major sync info and its CRC) in between left out. `None` if `b` ends
/// before the directory does.
fn parity(b: &[u8], substreams: usize, skip: usize) -> Option<bool> {
    let mut p = 0u8;
    for &x in b.get(..4)? {
        p ^= x;
    }
    let mut pos = 4 + skip;
    for _ in 0..substreams {
        let first = *b.get(pos)?;
        let n = if first >> 7 != 0 { 4 } else { 2 };
        for &x in b.get(pos..pos + n)? {
            p ^= x;
        }
        pos += n;
    }
    Some(((p >> 4) ^ p) & 0xF == 0xF)
}
