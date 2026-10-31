// Track C solver v2 — from v1 (hash-verified on full data).
// Byte state machine over the whole records.csv buffer, direct-array join
// on region_id, exact integer-cents money, interned names. Zero crates.
// Contract: reads /data/records.csv + /data/regions.csv, writes exactly
// one file /out/aggregates.txt, exits 0.
//
// v2 parallelizes the record scan without changing its parsing by one
// byte: the quote state at any buffer position equals the PARITY of '"'
// bytes before it (every quote toggles in/out; a "" escape toggles twice),
// so each thread can locate the first record start inside its chunk from
// the chunk's starting parity alone. Records are parsed by the identical
// field/delim state machine; aggregation is order-independent (integer
// sums, min/max, distinct name-id sets), so per-thread partial groups
// merge to exactly the single-threaded result. Name interning uses a
// 64-way sharded lock so the shared table does not serialize the scan.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------- FxHash

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default)]
struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn mix(&mut self, v: u64) {
        self.hash = (self.hash.rotate_left(5) ^ v).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.hash
    }

    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.mix(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.mix(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            self.mix(u16::from_le_bytes(bytes[..2].try_into().unwrap()) as u64);
            bytes = &bytes[2..];
        }
        if !bytes.is_empty() {
            self.mix(bytes[0] as u64);
        }
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
type FxSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;

// ---------------------------------------------------------------- input

// Exact-size read: the records file is hundreds of MB, and growing a Vec
// by doubling risks a transient double allocation at peak.
fn read_file(path: &str) -> Vec<u8> {
    let mut f = File::open(path).expect("open input");
    let len = f.metadata().expect("stat input").len() as usize;
    let mut buf = Vec::with_capacity(len);
    f.read_to_end(&mut buf).expect("read input");
    buf
}

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

// ---------------------------------------------------------------- numbers

// Strict digit run: 1-10 digits, all ASCII 0-9. Anything else panics —
// on generated data these fields are always clean digits, so a panic
// here means a corrupt file, never a legitimate edge case.
fn parse_uint(digits: &[u8]) -> u64 {
    if digits.is_empty() || digits.len() > 10 {
        panic!("bad integer field: {:?}", String::from_utf8_lossy(digits));
    }
    let mut v: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            panic!("bad integer field: {:?}", String::from_utf8_lossy(digits));
        }
        v = v * 10 + (c - b'0') as u64;
    }
    v
}

// Amount bytes: [-]digits.digits(2). Returns integer cents. -0.00 would
// give 0 here (sign * 0), which formats as 0.00, never -0.00.
fn parse_cents(value: &[u8]) -> i64 {
    let mut i = 0;
    let mut neg = false;
    if i < value.len() && value[i] == b'-' {
        neg = true;
        i += 1;
    }
    let int_start = i;
    while i < value.len() && value[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start || i - int_start > 10 {
        panic!("bad amount: {:?}", String::from_utf8_lossy(value));
    }
    let int_part = parse_uint(&value[int_start..i]);
    if i >= value.len() || value[i] != b'.' {
        panic!("bad amount: {:?}", String::from_utf8_lossy(value));
    }
    i += 1;
    if value.len() - i != 2 || !value[i].is_ascii_digit() || !value[i + 1].is_ascii_digit() {
        panic!("bad amount: {:?}", String::from_utf8_lossy(value));
    }
    let frac = (value[i] - b'0') as i64 * 10 + (value[i + 1] - b'0') as i64;
    let cents = int_part as i64 * 100 + frac;
    if neg {
        -cents
    } else {
        cents
    }
}

// Money formatting, integer-only: - if negative, no leading zeros,
// exactly two fraction digits. 0 -> "0.00", -5 -> "-0.05".
fn fmt_money(cents: i64, out: &mut String) {
    if cents < 0 {
        out.push('-');
    }
    let a = cents.unsigned_abs();
    out.push_str(&(a / 100).to_string());
    out.push('.');
    let f = a % 100;
    if f < 10 {
        out.push('0');
    }
    out.push_str(&f.to_string());
}

// ---------------------------------------------------------------- fields

// Scan one field starting at pos. Returns (quoted, unescaped_len, pos_after).
// Quoted: ends at the next '"' that is not part of a '""' pair; each '""'
// counts one byte. If scratch is Some it is cleared and filled with the
// unescaped bytes (quoted only; unquoted leaves it empty). Unquoted:
// runs to ','/CR/LF/EOF, value as-is, no trimming. A '"' inside an
// unquoted field panics (the grammar forbids it); an unterminated quoted
// field panics. pos_after points at the delimiter byte (or EOF) in both
// cases, so delimiter handling is uniform.
fn scan_field(
    buf: &[u8],
    pos: usize,
    scratch: Option<&mut Vec<u8>>,
) -> (bool, usize, usize) {
    let mut out = scratch;
    if let Some(s) = out.as_mut() {
        s.clear();
    }
    if pos < buf.len() && buf[pos] == b'"' {
        let mut p = pos + 1;
        let mut len = 0usize;
        loop {
            if p >= buf.len() {
                panic!("unterminated quoted field");
            }
            let c = buf[p];
            if c == b'"' {
                if p + 1 < buf.len() && buf[p + 1] == b'"' {
                    // Escaped quote: one literal '"' in the value.
                    if let Some(s) = out.as_mut() {
                        s.push(b'"');
                    }
                    len += 1;
                    p += 2;
                } else {
                    // Closing quote; pos_after is just past it.
                    return (true, len, p + 1);
                }
            } else {
                // Literal byte: comma, LF, CR, BOM bytes all count as data.
                if let Some(s) = out.as_mut() {
                    s.push(c);
                }
                len += 1;
                p += 1;
            }
        }
    }
    // Unquoted field.
    let mut p = pos;
    while p < buf.len() {
        let c = buf[p];
        if c == b',' || c == b'\r' || c == b'\n' {
            break;
        }
        if c == b'"' {
            panic!("quote inside unquoted field");
        }
        p += 1;
    }
    (false, p - pos, p)
}

// Delimiter after a field. Fields 1-4 must be followed by ','; field 5
// must be followed by CRLF, LF, or EOF. Anything else panics.
enum Delim {
    Comma(usize),
    End(usize),
}

fn delim(buf: &[u8], pos: usize, field_no: usize) -> Delim {
    if pos >= buf.len() {
        if field_no == 5 {
            return Delim::End(pos);
        }
        panic!("truncated record at field {}", field_no);
    }
    match buf[pos] {
        b',' => {
            if field_no == 5 {
                panic!("record has more than 5 fields");
            }
            Delim::Comma(pos + 1)
        }
        b'\r' => {
            if field_no != 5 {
                panic!("record ends early at field {}", field_no);
            }
            if pos + 1 >= buf.len() || buf[pos + 1] != b'\n' {
                panic!("bare CR outside quotes");
            }
            Delim::End(pos + 2)
        }
        b'\n' => {
            if field_no != 5 {
                panic!("record ends early at field {}", field_no);
            }
            Delim::End(pos + 1)
        }
        _ => panic!("unexpected byte after field {}", field_no),
    }
}

// Field value bytes: scratch (quoted) or the raw span (unquoted).
fn field_value<'a>(
    buf: &'a [u8],
    pos: usize,
    quoted: bool,
    value_len: usize,
    scratch: &'a [u8],
) -> &'a [u8] {
    if quoted {
        scratch
    } else {
        &buf[pos..pos + value_len]
    }
}

// ---------------------------------------------------------------- groups

struct Group {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    names: FxSet<u32>,
    name_bytes: u64,
    note_bytes: u64,
}

impl Group {
    fn new() -> Group {
        Group {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            names: FxSet::default(),
            name_bytes: 0,
            note_bytes: 0,
        }
    }
}

// Global name intern table, sharded by name hash so parallel scan threads
// can intern without serializing on a single lock. Ids only need to be
// distinct (groups store id sets and print the set length).
const NAME_SHARDS: usize = 64;

struct Names {
    shards: Vec<Mutex<FxMap<Box<[u8]>, u32>>>,
    next_id: AtomicU32,
}

impl Names {
    fn new() -> Names {
        Names {
            shards: (0..NAME_SHARDS)
                .map(|_| Mutex::new(FxMap::default()))
                .collect(),
            next_id: AtomicU32::new(0),
        }
    }

    fn intern(&self, bytes: &[u8]) -> u32 {
        let mut h = FxHasher::default();
        h.write(bytes);
        let shard = &self.shards[(h.finish() as usize) & (NAME_SHARDS - 1)];
        let mut m = shard.lock().expect("names shard lock");
        if let Some(&id) = m.get(bytes) {
            return id;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        m.insert(bytes.into(), id);
        id
    }
}

// ---------------------------------------------------------------- regions

// Parse regions.csv (plain ASCII, LF, header region_id,country).
// Returns (country codes in first-appearance order, lookup table indexed
// by region_id holding a country index or -1). Table size is max_id + 1,
// so in-range holes and above-range ids both land on the sentinel path.
fn parse_regions(buf: &[u8]) -> (Vec<[u8; 3]>, Vec<i32>) {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        match buf[pos..].iter().position(|&c| c == b'\n') {
            Some(off) => {
                lines.push(&buf[pos..pos + off]);
                pos += off + 1;
            }
            None => {
                lines.push(&buf[pos..]);
                pos = buf.len();
            }
        }
    }
    if lines.is_empty() {
        return (Vec::new(), vec![-1]);
    }
    let mut header = lines[0];
    if header.last() == Some(&b'\r') {
        header = &header[..header.len() - 1];
    }
    if header != b"region_id,country" {
        panic!("bad regions header");
    }
    let mut pairs: Vec<(u64, [u8; 3])> = Vec::new();
    let mut max_id: u64 = 0;
    for line in &lines[1..] {
        if line.is_empty() {
            continue; // trailing-LF artifact only; real lines are never empty
        }
        let mut country = *line;
        if country.last() == Some(&b'\r') {
            country = &country[..country.len() - 1];
        }
        let comma = match country.iter().position(|&c| c == b',') {
            Some(c) => c,
            None => panic!("bad regions line"),
        };
        let id = parse_uint(&country[..comma]);
        let code = &country[comma + 1..];
        if code.len() != 3 || !code.iter().all(|&c| c.is_ascii_uppercase()) {
            panic!("bad country code");
        }
        let mut arr = [0u8; 3];
        arr.copy_from_slice(code);
        if id > max_id {
            max_id = id;
        }
        pairs.push((id, arr));
    }
    let mut countries: Vec<[u8; 3]> = Vec::new();
    let mut table: Vec<i32> = vec![-1; max_id as usize + 1];
    for (id, code) in pairs {
        let idx = match countries.iter().position(|&c| c == code) {
            Some(i) => i as i32,
            None => {
                countries.push(code);
                (countries.len() - 1) as i32
            }
        };
        table[id as usize] = idx;
    }
    (countries, table)
}

// ---------------------------------------------------------------- parallel scan

// Quote state at a position is the parity of '"' bytes before it: every
// quote toggles in/out (a "" escape toggles twice), and in valid files
// quotes appear nowhere else. True = inside a quoted field.
fn quote_parity(buf: &[u8], start: usize, end: usize) -> bool {
    let mut inside = false;
    for &c in &buf[start..end] {
        if c == b'"' {
            inside = !inside;
        }
    }
    inside
}

// First record start at or after `from`, given the quote state at `from`.
// A record starts right after a terminator (LF or CRLF) outside quotes;
// `from` itself qualifies when it sits directly after a terminator (or is
// the first record position) with quotes closed. Panics on the same
// malformed inputs the record parser would reject (bare CR outside
// quotes, unterminated quoted field).
fn first_record_start(buf: &[u8], from: usize, first_pos: usize, inside: bool) -> usize {
    if !inside && (from == first_pos || buf[from - 1] == b'\n') {
        return from;
    }
    let mut p = from;
    let mut inq = inside;
    while p < buf.len() {
        let c = buf[p];
        if inq {
            if c == b'"' {
                inq = false;
            }
            p += 1;
        } else if c == b'"' {
            inq = true;
            p += 1;
        } else if c == b'\n' {
            return p + 1;
        } else if c == b'\r' {
            if p + 1 >= buf.len() || buf[p + 1] != b'\n' {
                panic!("bare CR outside quotes");
            }
            return p + 2;
        } else {
            p += 1;
        }
    }
    if inq {
        panic!("unterminated quoted field");
    }
    buf.len()
}

// Parse every record that STARTS in [start, range_end); a record may
// extend past range_end into a neighbouring chunk (the buffer is shared
// read-only, so this is safe). The loop body is the identical v1 state
// machine; only the group and name destinations are per-call.
fn scan_records(
    buf: &[u8],
    start: usize,
    range_end: usize,
    table: &[i32],
    unmatched_idx: usize,
    names: &Names,
    groups: &mut [Group],
    scratch: &mut Vec<u8>,
) -> u64 {
    let mut pos = start;
    let mut n_records = 0u64;
    while pos < range_end {
        // BOM immediately after a record terminator is skipped, once.
        // A second consecutive BOM would land in the id field and fail
        // the digit check below, exactly as the spec's invalid case.
        if buf[pos..].starts_with(BOM) {
            pos += BOM.len();
            if pos >= buf.len() {
                break;
            }
        }
        // Field 1: id.
        let (q1, v1, p1) = scan_field(buf, pos, Some(scratch));
        let id_bytes = field_value(buf, pos, q1, v1, scratch);
        let id = parse_uint(id_bytes) as i64;
        pos = match delim(buf, p1, 1) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 1"),
        };
        // Field 2: name (interned for distinct counting).
        let (q2, v2, p2) = scan_field(buf, pos, Some(scratch));
        let name_bytes = field_value(buf, pos, q2, v2, scratch);
        let name_id = names.intern(name_bytes);
        let name_len = v2 as u64;
        pos = match delim(buf, p2, 2) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 2"),
        };
        // Field 3: note (byte length only).
        let (_q3, v3, p3) = scan_field(buf, pos, None);
        let note_len = v3 as u64;
        pos = match delim(buf, p3, 3) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 3"),
        };
        // Field 4: region_id.
        let (q4, v4, p4) = scan_field(buf, pos, Some(scratch));
        let region_bytes = field_value(buf, pos, q4, v4, scratch);
        let region_id = parse_uint(region_bytes);
        pos = match delim(buf, p4, 4) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 4"),
        };
        // Field 5: amount.
        let (q5, v5, p5) = scan_field(buf, pos, Some(scratch));
        let amount_bytes = field_value(buf, pos, q5, v5, scratch);
        let cents = parse_cents(amount_bytes);
        pos = match delim(buf, p5, 5) {
            Delim::Comma(_) => panic!("record has more than 5 fields"),
            Delim::End(next) => next,
        };

        // ---- join + aggregate ----
        let gidx = if (region_id as usize) < table.len() && table[region_id as usize] >= 0 {
            table[region_id as usize] as usize
        } else {
            unmatched_idx
        };
        let g = &mut groups[gidx];
        g.count += 1;
        g.id_sum += id;
        g.amount_sum += cents;
        if cents < g.amount_min {
            g.amount_min = cents;
        }
        if cents > g.amount_max {
            g.amount_max = cents;
        }
        g.names.insert(name_id);
        g.name_bytes += name_len;
        g.note_bytes += note_len;
        n_records += 1;
    }
    n_records
}

// ---------------------------------------------------------------- main

fn main() {
    // ---- regions first (the join table) ----
    let rbuf = read_file("/data/regions.csv");
    let (countries, table) = parse_regions(&rbuf);
    let unmatched_idx = countries.len();
    let n_groups = countries.len() + 1;

    // ---- records: byte state machine over the whole buffer ----
    let buf = read_file("/data/records.csv");
    let mut pos = 0usize;
    // BOM at the very start of the file.
    if buf[pos..].starts_with(BOM) {
        pos += BOM.len();
    }
    // Header record: exactly id,name,note,region_id,amount.
    let mut scratch: Vec<u8> = Vec::new();
    let mut header_fields: Vec<Vec<u8>> = Vec::new();
    let mut hpos = pos;
    for field_no in 1..=5 {
        let (quoted, vlen, p2) = scan_field(&buf, hpos, Some(&mut scratch));
        let bytes: Vec<u8> = if quoted {
            scratch.clone()
        } else {
            buf[hpos..hpos + vlen].to_vec()
        };
        header_fields.push(bytes);
        match delim(&buf, p2, field_no) {
            Delim::Comma(next) => hpos = next,
            Delim::End(next) => {
                hpos = next;
                break;
            }
        }
    }
    if header_fields[0][..] != b"id"[..]
        || header_fields[1][..] != b"name"[..]
        || header_fields[2][..] != b"note"[..]
        || header_fields[3][..] != b"region_id"[..]
        || header_fields[4][..] != b"amount"[..]
    {
        panic!("bad records header");
    }
    pos = hpos;

    // ---- parallel scan: chunk by byte range, resync to a record start ----
    let nthreads = std::env::var("THREADS")
        .ok()
        .and_then(|t| t.parse::<usize>().ok())
        .filter(|&t| t >= 1)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8)
                .min(12)
        });
    // Spawning threads costs more than a small file is worth.
    let nthreads = if buf.len() - pos < 1 << 20 { 1 } else { nthreads };
    let n_records: u64;
    let mut merged: Vec<Group> = (0..n_groups).map(|_| Group::new()).collect();

    if nthreads == 1 {
        let names = Names::new();
        n_records = scan_records(
            &buf, pos, buf.len(), &table, unmatched_idx, &names, &mut merged, &mut scratch,
        );
    } else {
        // Chunk [pos, len) into nthreads byte ranges.
        let len = buf.len();
        let span = len - pos;
        let ranges: Vec<(usize, usize)> = (0..nthreads)
            .map(|i| {
                let start = pos + i * span / nthreads;
                let end = if i + 1 == nthreads {
                    len
                } else {
                    pos + (i + 1) * span / nthreads
                };
                (start, end)
            })
            .collect();

        // Pass 1 (parallel): quote parity of every range.
        let buf_ref = &buf;
        let parities: Vec<bool> = std::thread::scope(|s| {
            ranges
                .iter()
                .map(|&(start, end)| {
                    let buf_ref = buf_ref;
                    s.spawn(move || quote_parity(buf_ref, start, end))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().expect("parity thread"))
                .collect()
        });

        // Quote state at each range start (parity of everything before it).
        let states: Vec<bool> = {
            let mut v = Vec::with_capacity(nthreads + 1);
            let mut acc = quote_parity(&buf, 0, pos);
            v.push(acc);
            for &p in &parities {
                acc ^= p;
                v.push(acc);
            }
            v
        };

        // Pass 2 (parallel): parse records starting in each range.
        let names = Names::new();
        let table_ref = &table;
        let partials: Vec<(u64, Vec<Group>)> = std::thread::scope(|s| {
            let handles: Vec<_> = ranges
                .iter()
                .enumerate()
                .map(|(i, &(start, end))| {
                    let buf_ref = buf_ref;
                    let names_ref = &names;
                    let table_ref = table_ref;
                    let state = states[i];
                    s.spawn(move || {
                        let first = first_record_start(buf_ref, start, pos, state);
                        let mut groups: Vec<Group> = (0..n_groups).map(|_| Group::new()).collect();
                        let mut scratch = Vec::new();
                        let n = scan_records(
                            buf_ref, first, end, table_ref, unmatched_idx, names_ref,
                            &mut groups, &mut scratch,
                        );
                        (n, groups)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("scan thread"))
                .collect()
        });

        n_records = partials.iter().map(|p| p.0).sum();
        for (_, groups) in partials {
            merge_groups(&mut merged, groups);
        }
    }

    // ---- output: bytewise key sort, _UNMATCHED last by construction ----
    let mut keys: Vec<(&[u8], usize)> = Vec::new();
    for (i, code) in countries.iter().enumerate() {
        if merged[i].count > 0 {
            keys.push((code, i));
        }
    }
    if merged[unmatched_idx].count > 0 {
        keys.push((b"_UNMATCHED", unmatched_idx));
    }
    keys.sort_by(|a, b| a.0.cmp(b.0));

    let out = File::create("/out/aggregates.txt").expect("create output");
    let mut w = BufWriter::new(out);
    for (key, gi) in &keys {
        let g = &merged[*gi];
        let mut line = String::new();
        line.push_str(&String::from_utf8_lossy(key));
        line.push(' ');
        line.push_str(&g.count.to_string());
        line.push(' ');
        line.push_str(&g.id_sum.to_string());
        line.push(' ');
        fmt_money(g.amount_sum, &mut line);
        line.push(' ');
        fmt_money(g.amount_min, &mut line);
        line.push(' ');
        fmt_money(g.amount_max, &mut line);
        line.push(' ');
        line.push_str(&g.names.len().to_string());
        line.push(' ');
        line.push_str(&g.name_bytes.to_string());
        line.push(' ');
        line.push_str(&g.note_bytes.to_string());
        line.push('\n');
        w.write_all(line.as_bytes()).expect("write");
    }
    w.flush().expect("flush");
    eprintln!("records={} groups={}", n_records, keys.len());
}

// Merge per-thread partial groups into `merged`. All fields are pure
// integer arithmetic or set union, so the merge order cannot matter.
fn merge_groups(merged: &mut [Group], partial: Vec<Group>) {
    for (g, p) in merged.iter_mut().zip(partial) {
        g.count += p.count;
        g.id_sum += p.id_sum;
        g.amount_sum += p.amount_sum;
        g.amount_min = g.amount_min.min(p.amount_min);
        g.amount_max = g.amount_max.max(p.amount_max);
        g.names.extend(p.names);
        g.name_bytes += p.name_bytes;
        g.note_bytes += p.note_bytes;
    }
}