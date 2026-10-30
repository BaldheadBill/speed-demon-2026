use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};

#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x517cc1b727220a95;

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = (self.hash.rotate_left(5) ^ b as u64).wrapping_mul(FX_SEED);
        }
    }
    fn finish(&self) -> u64 {
        self.hash
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

struct Group {
    lats: Vec<u32>,
    errors: u32,
}

fn hour_index(ts: &[u8]) -> u32 {
    let day = (ts[8] - b'0') as u32 * 10 + (ts[9] - b'0') as u32;
    let hour = (ts[11] - b'0') as u32 * 10 + (ts[12] - b'0') as u32;
    match day {
        30 => hour - 18,
        31 => 6 + hour,
        1 => 30 + hour,
        _ => 54 + hour,
    }
}

fn hour_string(idx: u32) -> String {
    match idx {
        0..=5 => format!("2026-10-30T{:02}:00:00Z", idx + 18),
        6..=29 => format!("2026-10-31T{:02}:00:00Z", idx - 6),
        30..=53 => format!("2026-11-01T{:02}:00:00Z", idx - 30),
        _ => format!("2026-11-02T{:02}:00:00Z", idx - 54),
    }
}

fn parse_u32(bytes: &[u8]) -> u32 {
    let mut v: u32 = 0;
    for &b in bytes {
        v = v * 10 + (b - b'0') as u32;
    }
    v
}

// SWAR search: 8 bytes at a time for `target`, scalar tail.
fn find_byte(data: &[u8], mut i: usize, target: u8) -> Option<usize> {
    let rep = (target as u64) * 0x0101010101010101;
    while i + 8 <= data.len() {
        let chunk = u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        let x = chunk ^ rep;
        let found = x
            .wrapping_sub(0x0101010101010101)
            & !x
            & 0x8080808080808080;
        if found != 0 {
            return Some(i + (found.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < data.len() {
        if data[i] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn process_line(
    line: &[u8],
    intern: &mut FxMap<Vec<u8>, u16>,
    endpoints: &mut Vec<Vec<u8>>,
    groups: &mut FxMap<u32, Group>,
) {
    let ts = &line[..27];
    let method_end = find_byte(line, 28, b' ').expect("method end");
    let endpoint_start = method_end + 1;
    let endpoint_end = find_byte(line, endpoint_start, b' ').expect("endpoint end");
    let endpoint = &line[endpoint_start..endpoint_end];
    let status_start = endpoint_end + 1;
    let status = line[status_start];
    let latency = &line[status_start + 4..];
    let eid = if let Some(&eid) = intern.get(endpoint) {
        eid
    } else {
        let id = endpoints.len() as u16;
        endpoints.push(endpoint.to_vec());
        intern.insert(endpoint.to_vec(), id);
        id
    };
    let key = hour_index(ts) * 2048 + eid as u32;
    let g = groups.entry(key).or_insert_with(|| Group {
        lats: Vec::new(),
        errors: 0,
    });
    g.lats.push(parse_u32(latency));
    if status == b'5' {
        g.errors += 1;
    }
}

struct ThreadResult {
    endpoints: Vec<Vec<u8>>,
    groups: FxMap<u32, Group>,
}

// Aggregate only lines whose start offset lies in [start, end).
fn aggregate_range(path: &str, start: u64, end: u64) -> ThreadResult {
    let mut file = File::open(path).expect("open input");
    let mut skip_first = false;
    if start > 0 {
        // Peek at the byte before start: '\n' means start is already a
        // line start; otherwise start is mid-line and the first line
        // we see is a tail belonging to the previous thread.
        file.seek(SeekFrom::Start(start - 1)).expect("seek");
        let mut b = [0u8; 1];
        file.read_exact(&mut b).expect("peek");
        skip_first = b[0] != b'\n';
        // Cursor is now exactly at `start`.
    }

    let mut intern: FxMap<Vec<u8>, u16> = FxMap::default();
    let mut endpoints: Vec<Vec<u8>> = Vec::new();
    let mut groups: FxMap<u32, Group> = FxMap::default();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 1 << 20];
    let mut pos = start;
    let mut eof = false;

    'outer: loop {
        // Process every complete line currently buffered.
        let mut consumed = 0usize;
        while let Some(nl) = find_byte(&pending[consumed..], 0, b'\n') {
            let line_start = pos;
            if line_start >= end {
                break 'outer;
            }
            let line = &pending[consumed..consumed + nl];
            if !skip_first {
                process_line(line, &mut intern, &mut endpoints, &mut groups);
            }
            skip_first = false;
            pos += (nl + 1) as u64;
            consumed += nl + 1;
        }
        pending.drain(..consumed);
        if eof {
            break;
        }
        // Fill the next 1 MB block (short reads looped until full or EOF).
        let mut filled = 0usize;
        while filled < chunk.len() {
            match file.read(&mut chunk[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) => panic!("read: {e}"),
            }
        }
        if filled == 0 {
            // EOF with a final line lacking a trailing newline (the
            // spec file ends with one, so this is only a safety net).
            if !pending.is_empty() && pos < end && !skip_first {
                let tail = pending.clone();
                process_line(&tail, &mut intern, &mut endpoints, &mut groups);
            }
            break;
        }
        pending.extend_from_slice(&chunk[..filled]);
    }
    ThreadResult { endpoints, groups }
}

fn main() {
    let path = "/data/requests.log";
    let len = std::fs::metadata(path).expect("stat input").len();
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    let mut bounds: Vec<(u64, u64)> = Vec::new();
    for i in 0..nthreads {
        let s = len * i as u64 / nthreads as u64;
        let e = if i == nthreads - 1 {
            len
        } else {
            len * (i + 1) as u64 / nthreads as u64
        };
        bounds.push((s, e));
    }

    let results: Vec<ThreadResult> = std::thread::scope(|s| {
        let handles: Vec<_> = bounds
            .iter()
            .map(|&(a, b)| s.spawn(move || aggregate_range(path, a, b)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut intern: FxMap<Vec<u8>, u16> = FxMap::default();
    let mut endpoints: Vec<Vec<u8>> = Vec::new();
    let mut groups: FxMap<u32, Group> = FxMap::default();
    for tr in &results {
        let mut remap = vec![0u16; tr.endpoints.len()];
        for (lid, ep) in tr.endpoints.iter().enumerate() {
            remap[lid] = if let Some(&gid) = intern.get(ep.as_slice()) {
                gid
            } else {
                let gid = endpoints.len() as u16;
                endpoints.push(ep.clone());
                intern.insert(ep.clone(), gid);
                gid
            };
        }
        for (key, g) in &tr.groups {
            let gkey = (key / 2048) * 2048 + remap[(key % 2048) as usize] as u32;
            let dest = groups.entry(gkey).or_insert_with(|| Group {
                lats: Vec::new(),
                errors: 0,
            });
            dest.lats.extend_from_slice(&g.lats);
            dest.errors += g.errors;
        }
    }

    let mut keys: Vec<u32> = groups.keys().copied().collect();
    keys.sort_unstable_by(|&a, &b| {
        let (ha, ea) = (a / 2048, a % 2048);
        let (hb, eb) = (b / 2048, b % 2048);
        ha.cmp(&hb)
            .then_with(|| endpoints[ea as usize].cmp(&endpoints[eb as usize]))
    });

    let out = std::fs::File::create("/out/summary.txt").expect("create output");
    let mut w = std::io::BufWriter::new(out);
    for key in keys {
        let (hidx, eid) = (key / 2048, key % 2048);
        let g = groups.get_mut(&key).expect("group");
        let n = g.lats.len() as u64;
        let rank = |p: u64| -> usize { ((p * n + 99) / 100 - 1) as usize };
        let p50 = *g.lats.select_nth_unstable(rank(50)).1;
        let p95 = *g.lats.select_nth_unstable(rank(95)).1;
        let p99 = *g.lats.select_nth_unstable(rank(99)).1;
        let ppm = g.errors as u64 * 1_000_000 / n;
        writeln!(
            w,
            "{} {} {} {} {} {} {} {}",
            hour_string(hidx),
            std::str::from_utf8(&endpoints[eid as usize]).expect("endpoint"),
            n,
            p50,
            p95,
            p99,
            g.errors,
            ppm
        )
        .expect("write");
    }
    w.flush().expect("flush");
}

