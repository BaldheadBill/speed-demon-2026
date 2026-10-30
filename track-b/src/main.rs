// Track B solver v1 — single-threaded, from the Oct 28 design sketch.
// Pipeline: parse queries first -> scan docs once, posting lists for
// WANTED terms only -> BOOL left-to-right merges -> phrase anchor +
// grouped second scan -> results in query order.
// Correctness load-bearers (all from the frozen spec):
//  - byte tokenizer: tokens are maximal [a-z0-9] runs; every other byte
//    (including >= 0x80) is a separator. Docs buffer is lowercased in
//    place once, so token slices compare directly to lowercase terms.
//  - BOOL strictly left-to-right; NOT is binary subtraction.
//  - PHRASE = consecutive token positions (window check), not
//    co-occurrence; repeated terms need repeated consecutive tokens.
//  - Output: "<count> <sum> <up to 10 smallest ids>" or "0 0".
//  - No positions stored, no raw texts stored (v1 simplicity; the second
//    scan re-tokenizes from the in-memory docs buffer).
// Deviations from the design sketch, both deliberate:
//  - Per-doc dedup uses wanted-first lookup + a small per-doc vec instead
//    of sorting each doc's tokens (fewer comparisons: only wanted tokens
//    ever enter the dedup set).
//  - If doc ids ever arrive non-monotonic (spec says they won't), posting
//    lists and phrase match lists are sorted after the fact rather than
//    trusting scan order.
// Format correction found on the first run (Oct 30, 2026): queries.txt
// lines carry a type prefix, "BOOL " or "PHRASE ", before the expression.
// The parser strips it (space or tab separator); a bare expression with
// no prefix still parses via the fallback below.

use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Read, Write};
use std::time::Instant;

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

// ---------------------------------------------------------------- input

fn read_file(path: &str) -> Vec<u8> {
    let mut f = File::open(path).expect("open input");
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).expect("read input");
    buf
}

// Next line (without the \n) from buf starting at *pos; None at EOF.
// Handles a final line with no trailing newline.
fn next_line<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos >= buf.len() {
        return None;
    }
    let start = *pos;
    match buf[*pos..].iter().position(|&c| c == b'\n') {
        Some(off) => {
            *pos += off + 1;
            Some(&buf[start..start + off])
        }
        None => {
            *pos = buf.len();
            Some(&buf[start..])
        }
    }
}

// Next token slice from data starting at *pos; tokens are maximal
// [A-Za-z0-9] runs. (Docs buffer is lowercased before use, so slices
// come out lowercase; this function itself does not lowercase.)
fn next_token<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let n = data.len();
    let mut i = *pos;
    while i < n && !data[i].is_ascii_alphanumeric() {
        i += 1;
    }
    let start = i;
    while i < n && data[i].is_ascii_alphanumeric() {
        i += 1;
    }
    *pos = i;
    if start < i {
        Some(&data[start..i])
    } else {
        None
    }
}

// ---------------------------------------------------------------- queries

#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Not,
}

enum Query {
    Bool { terms: Vec<u32>, ops: Vec<Op> },
    Phrase { terms: Vec<u32> },
    Dup(usize),
}

struct ParsedQueries {
    queries: Vec<Query>,
    wanted_terms: Vec<Box<[u8]>>,
    n_bool: usize,
    n_phrase: usize,
    n_dup: usize,
}

fn parse_queries(buf: &[u8]) -> ParsedQueries {
    // Collect non-empty lines, stripping one trailing \r each.
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut pos = 0;
    while let Some(line) = next_line(buf, &mut pos) {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if !line.is_empty() {
            lines.push(line);
        }
    }
    // Optional count header: a lone integer equal to the remaining count.
    if !lines.is_empty() {
        let first = lines[0];
        if !first.is_empty()
            && first.iter().all(|&c| c.is_ascii_digit())
            && first
                .iter()
                .fold(0usize, |a, &c| a * 10 + (c - b'0') as usize)
                == lines.len() - 1
        {
            lines.remove(0);
        }
    }

    let mut wanted: FxMap<Box<[u8]>, u32> = FxMap::default();
    let mut wanted_terms: Vec<Box<[u8]>> = Vec::new();
    let mut term_idx = |tok: &[u8],
                        wanted: &mut FxMap<Box<[u8]>, u32>,
                        wanted_terms: &mut Vec<Box<[u8]>>|
     -> u32 {
        if let Some(&i) = wanted.get(tok) {
            return i;
        }
        let i = wanted_terms.len() as u32;
        wanted.insert(tok.into(), i);
        wanted_terms.push(tok.into());
        i
    };

    let mut dup_map: FxMap<Vec<u8>, usize> = FxMap::default();
    let mut queries: Vec<Query> = Vec::new();
    let mut n_bool = 0;
    let mut n_phrase = 0;
    let mut n_dup = 0;

    // Strip an optional "BOOL " / "PHRASE " type prefix (space or tab).
    let strip_prefix = |line: &[u8], tag: &[u8]| -> Option<usize> {
        if line.len() > tag.len()
            && &line[..tag.len()] == tag
            && (line[tag.len()] == b' ' || line[tag.len()] == b'\t')
        {
            Some(tag.len() + 1)
        } else {
            None
        }
    };
    for raw_line in lines {
        if let Some(&first) = dup_map.get(raw_line) {
            queries.push(Query::Dup(first));
            n_dup += 1;
            continue;
        }
        dup_map.insert(raw_line.to_vec(), queries.len());
        let line = if let Some(off) = strip_prefix(raw_line, b"PHRASE") {
            &raw_line[off..]
        } else if let Some(off) = strip_prefix(raw_line, b"BOOL") {
            &raw_line[off..]
        } else {
            raw_line
        };
        if line.first() == Some(&b'"') {
            // PHRASE: "..." with terms split on single spaces.
            if line.len() < 2 || line.last() != Some(&b'"') {
                panic!("malformed phrase line: {}", String::from_utf8_lossy(line));
            }
            let inner = &line[1..line.len() - 1];
            let mut terms = Vec::new();
            for tok in inner.split(|&c| c == b' ' || c == b'\t') {
                if !tok.is_empty() {
                    terms.push(term_idx(tok, &mut wanted, &mut wanted_terms));
                }
            }
            queries.push(Query::Phrase { terms });
            n_phrase += 1;
        } else {
            // BOOL: term (OP term)*, operators exactly AND / OR / NOT.
            let toks: Vec<&[u8]> = line
                .split(|&c| c == b' ' || c == b'\t')
                .filter(|t| !t.is_empty())
                .collect();
            if toks.is_empty() {
                panic!("empty bool line");
            }
            let is_op = |t: &[u8]| t == b"AND" || t == b"OR" || t == b"NOT";
            if is_op(toks[0]) {
                panic!(
                    "bool line starts with operator: {}",
                    String::from_utf8_lossy(line)
                );
            }
            let mut terms = vec![term_idx(toks[0], &mut wanted, &mut wanted_terms)];
            let mut ops = Vec::new();
            let mut i = 1;
            while i < toks.len() {
                let op = match toks[i] {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => panic!(
                        "expected operator, got term: {}",
                        String::from_utf8_lossy(line)
                    ),
                };
                if i + 1 >= toks.len() {
                    panic!(
                        "bool line ends with operator: {}",
                        String::from_utf8_lossy(line)
                    );
                }
                if is_op(toks[i + 1]) {
                    panic!(
                        "two operators in a row: {}",
                        String::from_utf8_lossy(line)
                    );
                }
                ops.push(op);
                terms.push(term_idx(toks[i + 1], &mut wanted, &mut wanted_terms));
                i += 2;
            }
            queries.push(Query::Bool { terms, ops });
            n_bool += 1;
        }
    }

    ParsedQueries {
        queries,
        wanted_terms,
        n_bool,
        n_phrase,
        n_dup,
    }
}

// ---------------------------------------------------------------- merges

// Intersect sorted a, b into out (cleared first). Galloping-lite when one
// side is >8x the other: walk the small side, binary-search the large
// side from an advancing start. Otherwise two-pointer.
fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    if a.is_empty() || b.is_empty() {
        return;
    }
    if a.len() * 8 < b.len() {
        let mut start = 0;
        for &x in a {
            match b[start..].binary_search(&x) {
                Ok(idx) => {
                    out.push(x);
                    start += idx + 1;
                }
                Err(idx) => {
                    start += idx;
                }
            }
        }
    } else if b.len() * 8 < a.len() {
        let mut start = 0;
        for &x in b {
            match a[start..].binary_search(&x) {
                Ok(idx) => {
                    out.push(x);
                    start += idx + 1;
                }
                Err(idx) => {
                    start += idx;
                }
            }
        }
    } else {
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                out.push(a[i]);
                i += 1;
                j += 1;
            } else if a[i] < b[j] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
}

fn union_merge(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

fn subtract(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
}

fn format_result(ids: &[u32]) -> String {
    if ids.is_empty() {
        return "0 0".to_string();
    }
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    let mut s = format!("{} {}", ids.len(), sum);
    for &x in ids.iter().take(10) {
        s.push(' ');
        s.push_str(&x.to_string());
    }
    s
}

// ---------------------------------------------------------------- main

struct PhraseData {
    query_idx: usize,
    terms: Vec<u32>,
}

fn main() {
    let t0 = Instant::now();

    // ---- parse queries first (wanted terms come from here) ----
    let qbuf = read_file("/data/queries.txt");
    let parsed = parse_queries(&qbuf);
    let t_parse = t0.elapsed();
    let n_queries = parsed.queries.len();
    eprintln!(
        "queries: total={} bool={} phrase={} dup={} wanted_terms={}",
        n_queries,
        parsed.n_bool,
        parsed.n_phrase,
        parsed.n_dup,
        parsed.wanted_terms.len()
    );

    // Wanted lookup table: term bytes -> posting index.
    let mut wanted: FxMap<Box<[u8]>, u32> = FxMap::default();
    for (i, t) in parsed.wanted_terms.iter().enumerate() {
        wanted.insert(t.clone(), i as u32);
    }
    let mut postings: Vec<Vec<u32>> = vec![Vec::new(); parsed.wanted_terms.len()];

    // ---- scan docs once ----
    let mut dbuf = read_file("/data/docs.txt");
    // Lowercase in place: A-Z -> a-z. Token boundaries are unchanged
    // (both cases are token bytes), so slices compare directly.
    for b in dbuf.iter_mut() {
        if b.is_ascii_uppercase() {
            *b |= 0x20;
        }
    }
    let mut n_docs = 0usize;
    let mut max_id = 0u32;
    let mut prev_id: Option<u32> = None;
    let mut monotonic = true;
    let mut skipped_no_tab = 0usize;
    let mut doc_terms: Vec<u32> = Vec::new();
    let mut pos = 0;
    while let Some(line) = next_line(&dbuf, &mut pos) {
        let tab = match line.iter().position(|&c| c == b'\t') {
            Some(t) => t,
            None => {
                skipped_no_tab += 1;
                continue;
            }
        };
        let id: u32 = line[..tab]
            .iter()
            .fold(0u32, |a, &c| a * 10 + (c - b'0') as u32);
        if let Some(p) = prev_id {
            if id < p {
                monotonic = false;
            }
        }
        prev_id = Some(id);
        if id > max_id {
            max_id = id;
        }
        n_docs += 1;
        let text = &line[tab + 1..];
        doc_terms.clear();
        let mut tpos = 0;
        while let Some(tok) = next_token(text, &mut tpos) {
            if let Some(&ti) = wanted.get(tok) {
                if !doc_terms.contains(&ti) {
                    doc_terms.push(ti);
                }
            }
        }
        for &ti in &doc_terms {
            postings[ti as usize].push(id);
        }
    }
    if !monotonic {
        // Spec says ids arrive ascending; if they ever don't, sort the
        // lists rather than trusting scan order.
        for list in postings.iter_mut() {
            list.sort_unstable();
        }
    }
    let n_doc_ids = max_id as usize + 1;
    let posting_total: usize = postings.iter().map(|l| l.len()).sum();
    let t_scan = t0.elapsed();
    eprintln!(
        "docs: lines={} max_id={} skipped_no_tab={} monotonic={} postings_total={}",
        n_docs, max_id, skipped_no_tab, monotonic, posting_total
    );

    let mut results: Vec<String> = vec![String::new(); n_queries];

    // ---- BOOL queries ----
    let mut buf_a: Vec<u32> = Vec::new();
    let mut buf_b: Vec<u32> = Vec::new();
    for (qi, q) in parsed.queries.iter().enumerate() {
        if let Query::Bool { terms, ops } = q {
            if ops.is_empty() {
                // Single term: use the posting list directly, no copy.
                results[qi] = format_result(&postings[terms[0] as usize]);
            } else {
                buf_a.clear();
                buf_a.extend_from_slice(&postings[terms[0] as usize]);
                let mut cur_is_a = true;
                for (k, op) in ops.iter().enumerate() {
                    let list = &postings[terms[k + 1] as usize];
                    if cur_is_a {
                        match op {
                            Op::And => intersect(&buf_a, list, &mut buf_b),
                            Op::Or => union_merge(&buf_a, list, &mut buf_b),
                            Op::Not => subtract(&buf_a, list, &mut buf_b),
                        }
                    } else {
                        match op {
                            Op::And => intersect(&buf_b, list, &mut buf_a),
                            Op::Or => union_merge(&buf_b, list, &mut buf_a),
                            Op::Not => subtract(&buf_b, list, &mut buf_a),
                        }
                    }
                    cur_is_a = !cur_is_a;
                }
                let res = if cur_is_a { &buf_a } else { &buf_b };
                results[qi] = format_result(res);
            }
        }
    }
    let t_bool = t0.elapsed();

    // ---- PHRASE queries: anchor + grouped second scan ----
    let anchor_thresh = n_doc_ids / 1000;
    let mut phrases: Vec<PhraseData> = Vec::new();
    let mut phrase_matches: Vec<Vec<u32>> = Vec::new();
    let mut phrases_by_doc: Vec<Vec<u32>> = vec![Vec::new(); n_doc_ids];
    for (qi, q) in parsed.queries.iter().enumerate() {
        if let Query::Phrase { terms } = q {
            if terms.is_empty() {
                results[qi] = "0 0".to_string();
                continue;
            }
            // Absent term -> no match, no grouping.
            if terms.iter().any(|&t| postings[t as usize].is_empty()) {
                results[qi] = "0 0".to_string();
                continue;
            }
            let (mut min_t, mut min_len) = (terms[0], postings[terms[0] as usize].len());
            for &t in &terms[1..] {
                let l = postings[t as usize].len();
                if l < min_len {
                    min_t = t;
                    min_len = l;
                }
            }
            let ppos = phrases.len();
            phrases.push(PhraseData {
                query_idx: qi,
                terms: terms.clone(),
            });
            phrase_matches.push(Vec::new());
            if min_len <= anchor_thresh {
                // Anchor path: candidates = anchor posting list.
                for &doc in &postings[min_t as usize] {
                    phrases_by_doc[doc as usize].push(ppos as u32);
                }
            } else {
                // No anchor: intersect all phrase terms' lists first.
                let mut cand = postings[min_t as usize].clone();
                let mut tmp: Vec<u32> = Vec::new();
                for &t in terms {
                    if t == min_t {
                        continue;
                    }
                    intersect(&cand, &postings[t as usize], &mut tmp);
                    std::mem::swap(&mut cand, &mut tmp);
                    if cand.is_empty() {
                        break;
                    }
                }
                for doc in cand {
                    phrases_by_doc[doc as usize].push(ppos as u32);
                }
            }
        }
    }

    // Second scan in doc order; matches append ascending (unless the
    // file was non-monotonic, in which case sort at the end).
    let mut pos2 = 0;
    let mut tokens: Vec<&[u8]> = Vec::new();
    while let Some(line) = next_line(&dbuf, &mut pos2) {
        let tab = match line.iter().position(|&c| c == b'\t') {
            Some(t) => t,
            None => continue,
        };
        let id: u32 = line[..tab]
            .iter()
            .fold(0u32, |a, &c| a * 10 + (c - b'0') as u32);
        let assigned = &phrases_by_doc[id as usize];
        if assigned.is_empty() {
            continue;
        }
        let text = &line[tab + 1..];
        tokens.clear();
        let mut tpos = 0;
        while let Some(tok) = next_token(text, &mut tpos) {
            tokens.push(tok);
        }
        for &ppos in assigned {
            let ph = &phrases[ppos as usize];
            let plen = ph.terms.len();
            if tokens.len() < plen {
                continue;
            }
            // Window check: phrase terms at consecutive token positions.
            let mut found = false;
            for start in 0..=(tokens.len() - plen) {
                let mut ok = true;
                for j in 0..plen {
                    if tokens[start + j] != parsed.wanted_terms[ph.terms[j] as usize].as_ref()
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    found = true;
                    break;
                }
            }
            if found {
                phrase_matches[ppos as usize].push(id);
            }
        }
    }
    if !monotonic {
        for m in phrase_matches.iter_mut() {
            m.sort_unstable();
        }
    }
    for (ppos, ph) in phrases.iter().enumerate() {
        results[ph.query_idx] = format_result(&phrase_matches[ppos]);
    }
    let t_phrase = t0.elapsed();

    // ---- write in query order ----
    let out = File::create("/out/results.txt").expect("create output");
    let mut w = BufWriter::new(out);
    for (qi, q) in parsed.queries.iter().enumerate() {
        if let Query::Dup(first) = q {
            w.write_all(results[*first].as_bytes()).expect("write");
        } else {
            w.write_all(results[qi].as_bytes()).expect("write");
        }
        w.write_all(b"\n").expect("write");
    }
    w.flush().expect("flush");
    eprintln!(
        "done: parse={:?} scan={:?} bool={:?} phrase={:?} total={:?}",
        t_parse,
        t_scan - t_parse,
        t_bool - t_scan,
        t_phrase - t_bool,
        t0.elapsed()
    );
}
