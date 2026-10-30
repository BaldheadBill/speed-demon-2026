// Track C solver v1 — written fresh in the 72-hour window (no October code).
// Byte state machine over the whole records.csv buffer, direct-array join
// on region_id, exact integer-cents money, interned names. Zero crates.
// Contract: reads /data/records.csv + /data/regions.csv, writes exactly
// one file /out/aggregates.txt, exits 0. Single-threaded by design: the
// byte scan, not the join, is the bottleneck, and a hostile CSV split
// across threads is hash risk for a stretch track.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Read, Write};

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

fn read_file(path: &str) -> Vec<u8> {
    let mut f = File::open(path).expect("open input");
    let mut buf = Vec::new();
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

// Intern a name's exact bytes; returns its small integer id. The empty
// name (unquoted empty and "" alike) interns once, like any other value.
fn intern_name(map: &mut FxMap<Box<[u8]>, u32>, bytes: &[u8]) -> u32 {
    if let Some(&id) = map.get(bytes) {
        return id;
    }
    let id = map.len() as u32;
    map.insert(bytes.into(), id);
    id
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

// ---------------------------------------------------------------- main

fn main() {
    // ---- regions first (the join table) ----
    let rbuf = read_file("/data/regions.csv");
    let (countries, table) = parse_regions(&rbuf);
    let unmatched_idx = countries.len();
    let mut groups: Vec<Group> = Vec::new();
    for _ in 0..=countries.len() {
        groups.push(Group::new());
    }

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

    let mut names_map: FxMap<Box<[u8]>, u32> = FxMap::default();
    let mut n_records: u64 = 0;
    while pos < buf.len() {
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
        let (q1, v1, p1) = scan_field(&buf, pos, Some(&mut scratch));
        let id_bytes = field_value(&buf, pos, q1, v1, &scratch);
        let id = parse_uint(id_bytes) as i64;
        pos = match delim(&buf, p1, 1) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 1"),
        };
        // Field 2: name (interned for distinct counting).
        let (q2, v2, p2) = scan_field(&buf, pos, Some(&mut scratch));
        let name_bytes = field_value(&buf, pos, q2, v2, &scratch);
        let name_id = intern_name(&mut names_map, name_bytes);
        let name_len = v2 as u64;
        pos = match delim(&buf, p2, 2) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 2"),
        };
        // Field 3: note (byte length only).
        let (_q3, v3, p3) = scan_field(&buf, pos, None);
        let note_len = v3 as u64;
        pos = match delim(&buf, p3, 3) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 3"),
        };
        // Field 4: region_id.
        let (q4, v4, p4) = scan_field(&buf, pos, Some(&mut scratch));
        let region_bytes = field_value(&buf, pos, q4, v4, &scratch);
        let region_id = parse_uint(region_bytes);
        pos = match delim(&buf, p4, 4) {
            Delim::Comma(next) => next,
            Delim::End(_) => panic!("record ends after field 4"),
        };
        // Field 5: amount.
        let (q5, v5, p5) = scan_field(&buf, pos, Some(&mut scratch));
        let amount_bytes = field_value(&buf, pos, q5, v5, &scratch);
        let cents = parse_cents(amount_bytes);
        pos = match delim(&buf, p5, 5) {
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

    // ---- output: bytewise key sort, _UNMATCHED last by construction ----
    let mut keys: Vec<(&[u8], usize)> = Vec::new();
    for (i, code) in countries.iter().enumerate() {
        if groups[i].count > 0 {
            keys.push((code, i));
        }
    }
    if groups[unmatched_idx].count > 0 {
        keys.push((b"_UNMATCHED", unmatched_idx));
    }
    keys.sort_by(|a, b| a.0.cmp(b.0));

    let out = File::create("/out/aggregates.txt").expect("create output");
    let mut w = BufWriter::new(out);
    for (key, gi) in &keys {
        let g = &groups[*gi];
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
