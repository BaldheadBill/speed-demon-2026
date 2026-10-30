use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

const FX_SEED: u64 = 0x517cc1b727220a95;

#[inline(always)]
fn fx_word(h: u64, word: u64) -> u64 {
    (h.rotate_left(37) ^ word).wrapping_mul(FX_SEED)
}

#[inline]
fn fx_hash(b: &[u8]) -> u64 {
    let mut h = 0u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = fx_word(h, u64::from_le_bytes(c.try_into().unwrap()));
    }
    let r = chunks.remainder();
    if !r.is_empty() {
        let mut tail = 0u64;
        for (i, &bb) in r.iter().enumerate() {
            tail |= (bb as u64) << (8 * i);
        }
        h = fx_word(h, tail);
    }
    h
}

struct FxHasher(u64);
impl Default for FxHasher {
    fn default() -> Self {
        FxHasher(0)
    }
}
impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0 = fx_hash(bytes);
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.0 = fx_word(self.0 ^ v, FX_SEED);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[inline(always)]
fn hour_index(ts: &[u8]) -> u32 {
    let day = ((ts[8] - b'0') as u32) * 10 + (ts[9] - b'0') as u32;
    let hour = ((ts[11] - b'0') as u32) * 10 + (ts[12] - b'0') as u32;
    match day {
        30 => hour - 18,
        31 => 6 + hour,
        1 => 30 + hour,
        _ => 54 + hour,
    }
}

fn build_hour_table() -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(72);
    for idx in 0..72u32 {
        let s = match idx {
            0..=5 => format!("2026-10-30T{:02}:00:00Z", idx + 18),
            6..=29 => format!("2026-10-31T{:02}:00:00Z", idx - 6),
            30..=53 => format!("2026-11-01T{:02}:00:00Z", idx - 30),
            _ => format!("2026-11-02T{:02}:00:00Z", idx - 54),
        };
        v.push(s);
    }
    v
}

#[inline(always)]
fn find_byte(data: &[u8], mut i: usize, target: u8) -> Option<usize> {
    let rep = (target as u64) * 0x0101010101010101;
    while i + 8 <= data.len() {
        let x = u64::from_le_bytes(data[i..i + 8].try_into().unwrap()) ^ rep;
        let found = x
            .wrapping_sub(0x0101010101010101)
            & !x
            & 0x8080808080808080;
        if found != 0 {
            return Some(i + (found.trailing_zeros() >> 3) as usize);
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

#[inline(always)]
fn find_two_spaces(data: &[u8], mut i: usize) -> (usize, usize) {
    const LOM: u64 = 0x0101010101010101;
    const HIM: u64 = 0x8080808080808080;
    let rep = 0x2020202020202020u64;
    let mut first = usize::MAX;
    while i + 8 <= data.len() {
        let x = u64::from_le_bytes(data[i..i + 8].try_into().unwrap()) ^ rep;
        let found = x.wrapping_sub(LOM) & !x & HIM;
        if found != 0 {
            let b = (found.trailing_zeros() >> 3) as usize;
            if first == usize::MAX {
                first = i + b;
                let rest = if b == 7 {
                    0
                } else {
                    found & !((1u64 << (8 * (b as u32 + 1))) - 1)
                };
                if rest != 0 {
                    return (first, i + (rest.trailing_zeros() >> 3) as usize);
                }
            } else {
                return (first, i + b);
            }
        }
        i += 8;
    }
    while i < data.len() {
        if data[i] == b' ' {
            if first == usize::MAX {
                first = i;
            } else {
                return (first, i);
            }
        }
        i += 1;
    }
    unreachable!("grammar guarantees two spaces after ts")
}

#[inline(always)]
fn parse8(x: u64) -> u64 {
    const E: u64 = 0x00FF00FF00FF00FF;
    const H: u64 = 0x0000FFFF0000FFFF;
    let pairs = (x & E).wrapping_mul(10) + ((x >> 8) & E);
    let quads = pairs.wrapping_mul(100);
    let quads_lo = (quads & H) + ((pairs >> 16) & H);
    let v = quads_lo.wrapping_mul(10000);
    let v = (v & 0xFFFF_FFFF) + (quads_lo >> 32);
    v & 0xFFFF_FFFF
}

#[inline]
fn parse_latency(b: &[u8]) -> u64 {
    let n = b.len();
    let mut v = 0u64;
    let mut i = 0usize;
    if n >= 8 {
        let chunk = u64::from_le_bytes(b[..8].try_into().unwrap());
        v = parse8(chunk.wrapping_sub(0x3030303030303030));
        i = 8;
    }
    while i < n {
        v = v * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    v
}

enum Lats {
    Small([u32; 4], u8),
    Big(Vec<u32>),
}

impl Lats {
    fn new(lat: u32) -> Self {
        Lats::Small([lat, 0, 0, 0], 1)
    }
    fn push(&mut self, lat: u32) {
        match self {
            Lats::Small(arr, n) => {
                let nn = *n as usize;
                if nn < 4 {
                    arr[nn] = lat;
                    *n += 1;
                } else {
                    let mut v = Vec::with_capacity(8);
                    v.extend_from_slice(arr);
                    v.push(lat);
                    *self = Lats::Big(v);
                }
            }
            Lats::Big(v) => v.push(lat),
        }
    }
    fn len(&self) -> u64 {
        match self {
            Lats::Small(_, n) => *n as u64,
            Lats::Big(v) => v.len() as u64,
        }
    }
    fn as_slice(&self) -> &[u32] {
        match self {
            Lats::Small(arr, n) => &arr[..*n as usize],
            Lats::Big(v) => v.as_slice(),
        }
    }
    fn into_vec(self) -> Vec<u32> {
        match self {
            Lats::Small(arr, n) => arr[..n as usize].to_vec(),
            Lats::Big(v) => v,
        }
    }
}

const SHARD_BITS: usize = 10; // 1024 shards
const SHARD_MASK: usize = (1 << SHARD_BITS) - 1;
const NO_NEXT: u32 = u32::MAX;

const HOT_BITS: usize = 10; // 1024 hot-cache slots per thread
const HOT_MASK: usize = (1 << HOT_BITS) - 1;

struct Slot {
    next: u32,
    off: u32,
    len: u32,
    hour: u8,
    errors: u32,
    lats: Lats,
}

struct ShardInner {
    map: FxMap<u64, u32>,
    slots: Vec<Slot>,
    arena: Vec<u8>,
}

impl ShardInner {
    fn new() -> Self {
        ShardInner {
            map: FxMap::default(),
            slots: Vec::new(),
            arena: Vec::new(),
        }
    }
}

type Shards = Vec<Mutex<ShardInner>>;

fn new_shards() -> Shards {
    (0..1 << SHARD_BITS)
        .map(|_| Mutex::new(ShardInner::new()))
        .collect()
}

struct HotSlot {
    hour: u8,
    occupied: bool,
    errors: u32,
    ep: Vec<u8>,
    lats: Lats,
}

impl HotSlot {
    fn empty() -> Self {
        HotSlot {
            hour: 0,
            occupied: false,
            errors: 0,
            ep: Vec::new(),
            lats: Lats::Small([0; 4], 0),
        }
    }
}

/// Append one whole cached group to the global table under a single
/// lock acquisition. Used for cache evictions and the end-of-thread
/// flush, never on the per-line path.
fn insert_group(shards: &Shards, hour: u32, ep: &[u8], lats: Lats, errors: u32) {
    let h = fx_hash(ep);
    let key = ((hour as u64) << 57) | (h & ((1 << 57) - 1));
    let shard_idx = ((h ^ (hour as u64).wrapping_mul(0x9E3779B97F4A7C15)) as usize) & SHARD_MASK;
    let shard = &shards[shard_idx];

    let mut guard = shard.lock().expect("shard lock");
    let inner = &mut *guard;
    match inner.map.entry(key) {
        Entry::Vacant(v) => {
            let off = inner.arena.len() as u32;
            inner.arena.extend_from_slice(ep);
            inner.slots.push(Slot {
                next: NO_NEXT,
                off,
                len: ep.len() as u32,
                hour: hour as u8,
                errors,
                lats,
            });
            v.insert(inner.slots.len() as u32 - 1);
        }
        Entry::Occupied(mut o) => {
            let mut sidx = *o.get();
            loop {
                let (next, matched) = {
                    let s = &inner.slots[sidx as usize];
                    let m = s.len as usize == ep.len()
                        && inner.arena[s.off as usize..s.off as usize + s.len as usize] == ep[..];
                    (s.next, m)
                };
                if matched {
                    let s = &mut inner.slots[sidx as usize];
                    for &lat in lats.as_slice() {
                        s.lats.push(lat);
                    }
                    s.errors += errors;
                    return;
                }
                if next == NO_NEXT {
                    let off = inner.arena.len() as u32;
                    inner.arena.extend_from_slice(ep);
                    let new_idx = inner.slots.len() as u32;
                    inner.slots.push(Slot {
                        next: *o.get(),
                        off,
                        len: ep.len() as u32,
                        hour: hour as u8,
                        errors,
                        lats,
                    });
                    *o.get_mut() = new_idx;
                    return;
                }
                sidx = next;
            }
        }
    }
}

/// Feed one line through the thread's hot cache. Hits never touch a
/// lock or the global table; misses flush the evicted slot (batched)
/// and cache the new line.
fn feed_line(line: &[u8], hot: &mut [HotSlot], shards: &Shards) {
    let hour = hour_index(line);
    let (m_end, e_end) = find_two_spaces(line, 28);
    let ep = &line[m_end + 1..e_end];
    let err5 = if line[e_end + 1] == b'5' { 1 } else { 0 };
    let lat = parse_latency(&line[e_end + 5..]) as u32;

    let h = fx_hash(ep);
    // Cache slot uses different hash bits than the shard index so the
    // two mappings do not systematically coincide.
    let cslot = (((h ^ (hour as u64).wrapping_mul(0x9E3779B97F4A7C15)) >> 10) as usize) & HOT_MASK;

    let hit = {
        let slot = &hot[cslot];
        slot.occupied && slot.hour == hour as u8 && slot.ep.as_slice() == ep
    };
    if hit {
        let slot = &mut hot[cslot];
        slot.lats.push(lat);
        slot.errors += err5;
        return;
    }
    // Miss: flush the evicted occupant, then cache this line.
    let old = std::mem::replace(&mut hot[cslot], HotSlot::empty());
    let HotSlot {
        hour: old_hour,
        occupied,
        errors: old_errors,
        ep: old_ep,
        lats: old_lats,
    } = old;
    if occupied {
        insert_group(shards, old_hour as u32, &old_ep, old_lats, old_errors);
    }
    let slot = &mut hot[cslot];
    slot.hour = hour as u8;
    slot.occupied = true;
    slot.errors = err5;
    slot.ep.extend_from_slice(ep);
    slot.lats = Lats::new(lat);
}

const CHUNK: usize = 1 << 20; // 1 MB read blocks

#[inline]
fn read_full(file: &mut File, chunk: &mut [u8], want: usize) -> usize {
    let mut filled = 0usize;
    while filled < want {
        match file.read(&mut chunk[filled..want]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => panic!("read: {e}"),
        }
    }
    filled
}

/// Parse every complete line whose start offset lies in [start, end).
fn aggregate_range(path: &str, start: u64, end: u64, shards: &Shards) {
    let mut file = File::open(path).expect("open input");
    let mut skip_first = false;
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1)).expect("seek");
        let mut b = [0u8; 1];
        file.read_exact(&mut b).expect("peek");
        skip_first = b[0] != b'\n';
    }

    let mut hot: Vec<HotSlot> = (0..1 << HOT_BITS).map(|_| HotSlot::empty()).collect();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut pos = start;
    'outer: loop {
        let mut consumed = 0usize;
        while let Some(nl) = find_byte(&pending[consumed..], 0, b'\n') {
            let line_start = pos;
            if line_start >= end {
                break 'outer;
            }
            let line = &pending[consumed..consumed + nl];
            if !skip_first {
                feed_line(line, &mut hot, shards);
            }
            skip_first = false;
            pos += (nl + 1) as u64;
            consumed += nl + 1;
        }
        pending.drain(..consumed);
        let filled = read_full(&mut file, &mut chunk, CHUNK);
        if filled == 0 {
            if !pending.is_empty() && pos < end && !skip_first {
                let tail = pending.clone();
                feed_line(&tail, &mut hot, shards);
            }
            break;
        }
        pending.extend_from_slice(&chunk[..filled]);
    }
    // End of thread: flush every cached group to the global table.
    for slot in hot {
        let HotSlot {
            hour,
            occupied,
            errors,
            ep,
            lats,
        } = slot;
        if occupied {
            insert_group(shards, hour as u32, &ep, lats, errors);
        }
    }
}

struct KeyRow {
    hour: u32,
    shard: u16,
    slot: u32,
    off: u32,
    len: u32,
}

fn push_int(buf: &mut Vec<u8>, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

fn main() {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());
    let path: &'static str = Box::leak(format!("{data_dir}/requests.log").into_boxed_str());
    let out_path = format!("{out_dir}/summary.txt");

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

    let shards = new_shards();
    let len = std::fs::metadata(path).expect("stat input").len();

    let mut bounds: Vec<(u64, u64)> = Vec::with_capacity(nthreads);
    for i in 0..nthreads {
        let s = len * i as u64 / nthreads as u64;
        let e = if i == nthreads - 1 {
            len
        } else {
            len * (i + 1) as u64 / nthreads as u64
        };
        bounds.push((s, e));
    }

    std::thread::scope(|s| {
        let shards_ref = &shards;
        for &(a, b) in &bounds {
            s.spawn(move || aggregate_range(path, a, b, shards_ref));
        }
    });

    let mut inners: Vec<ShardInner> = shards
        .into_iter()
        .map(|m| m.into_inner().expect("shard inner"))
        .collect();

    let total_keys: usize = inners.iter().map(|i| i.slots.len()).sum();
    let mut sort_arena: Vec<u8> = Vec::with_capacity(1 << 22);
    let mut rows: Vec<KeyRow> = Vec::with_capacity(total_keys);
    for (si, inner) in inners.iter().enumerate() {
        for (k, slot) in inner.slots.iter().enumerate() {
            rows.push(KeyRow {
                hour: slot.hour as u32,
                shard: si as u16,
                slot: k as u32,
                off: sort_arena.len() as u32,
                len: slot.len,
            });
            sort_arena.extend_from_slice(
                &inner.arena[slot.off as usize..slot.off as usize + slot.len as usize],
            );
        }
    }
    // Output order: hour ascending, then endpoint bytes ascending.
    rows.sort_unstable_by(|a, b| {
        a.hour
            .cmp(&b.hour)
            .then_with(|| {
                sort_arena[a.off as usize..a.off as usize + a.len as usize]
                    .cmp(&sort_arena[b.off as usize..b.off as usize + b.len as usize])
            })
    });

    let hour_table = build_hour_table();

    let out = File::create(&out_path).expect("create output");
    let mut w = std::io::BufWriter::with_capacity(1 << 16, out);
    let mut buf: Vec<u8> = Vec::with_capacity(192);

    for k in &rows {
        let slot = &mut inners[k.shard as usize].slots[k.slot as usize];
        let n = slot.lats.len() as u64;
        let errors = slot.errors;
        // Take the latencies out of the slot instead of copying them:
        // the slot is never used again after this row is written.
        let mut lat = std::mem::replace(&mut slot.lats, Lats::Small([0; 4], 0)).into_vec();
        let rank = |p: u64| -> usize { ((p * n + 99) / 100 - 1) as usize };
        let r99 = rank(99);
        let p99 = *lat.select_nth_unstable(r99).1;
        let r95 = rank(95).min(r99);
        let p95 = if r95 == r99 {
            p99
        } else {
            *lat[..r99].select_nth_unstable(r95).1
        };
        let r50 = rank(50).min(r95);
        let p50 = if r50 == r95 {
            p95
        } else {
            *lat[..r95].select_nth_unstable(r50).1
        };
        let ppm = errors as u64 * 1_000_000 / n;

        buf.clear();
        buf.extend_from_slice(hour_table[k.hour as usize].as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(&sort_arena[k.off as usize..k.off as usize + k.len as usize]);
        for v in [n, p50 as u64, p95 as u64, p99 as u64, errors as u64, ppm] {
            buf.push(b' ');
            push_int(&mut buf, v);
        }
        buf.push(b'\n');
        w.write_all(&buf).expect("write");
    }
    w.flush().expect("flush");
}