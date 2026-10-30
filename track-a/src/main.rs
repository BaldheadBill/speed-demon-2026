use std::collections::HashMap;
use std::fs;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Write};

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

struct ThreadResult<'a> {
    endpoints: Vec<&'a [u8]>,
    groups: FxMap<u32, Group>,
}

fn aggregate(data: &[u8]) -> ThreadResult<'_> {
    let mut intern: FxMap<&[u8], u16> = FxMap::default();
    let mut endpoints: Vec<&[u8]> = Vec::new();
    let mut groups: FxMap<u32, Group> = FxMap::default();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(|&b| b == b' ');
        let ts = parts.next().expect("ts");
        let _method = parts.next().expect("method");
        let endpoint = parts.next().expect("endpoint");
        let status = parts.next().expect("status");
        let latency = parts.next().expect("latency");
        let eid = *intern.entry(endpoint).or_insert_with(|| {
            endpoints.push(endpoint);
            (endpoints.len() - 1) as u16
        });
        let key = hour_index(ts) * 2048 + eid as u32;
        let g = groups.entry(key).or_insert_with(|| Group {
            lats: Vec::new(),
            errors: 0,
        });
        g.lats.push(parse_u32(latency));
        if status[0] == b'5' {
            g.errors += 1;
        }
    }
    ThreadResult { endpoints, groups }
}

fn main() {
    let data = fs::read("/data/requests.log").expect("read input");

    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    // Split at line boundaries: each chunk ends just after a '\n'.
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for i in 0..nthreads {
        if i == nthreads - 1 {
            bounds.push((start, data.len()));
            break;
        }
        let mut end = start + data.len() / nthreads;
        while end < data.len() && data[end] != b'\n' {
            end += 1;
        }
        if end < data.len() {
            end += 1;
        }
        bounds.push((start, end));
        start = end;
    }
	//altered from using move because it moved complete vector into 1st thread rather than reference it
    let data_slice: &[u8] = &data;

let results: Vec<ThreadResult> = std::thread::scope(|s| {
    let handles: Vec<_> = bounds
        .iter()
        .map(|&(a, b)| s.spawn(move || aggregate(&data_slice[a..b])))
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
});


    // Merge: remap each thread's local endpoint ids to global ids.
    let mut intern: FxMap<&[u8], u16> = FxMap::default();
    let mut endpoints: Vec<&[u8]> = Vec::new();
    let mut groups: FxMap<u32, Group> = FxMap::default();
    for tr in &results {
        let mut remap = vec![0u16; tr.endpoints.len()];
        for (lid, &ep) in tr.endpoints.iter().enumerate() {
            remap[lid] = *intern.entry(ep).or_insert_with(|| {
                endpoints.push(ep);
                (endpoints.len() - 1) as u16
            });
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
            .then_with(|| endpoints[ea as usize].cmp(endpoints[eb as usize]))
    });

    let out = fs::File::create("/out/summary.txt").expect("create output");
    let mut w = BufWriter::new(out);
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
            std::str::from_utf8(endpoints[eid as usize]).expect("endpoint"),
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
