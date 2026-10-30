use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::sync::Mutex;

#[cfg(unix)]
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
const PROT_READ: i32 = 1;
#[cfg(unix)]
const MAP_PRIVATE: i32 = 2;

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

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

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
    fn sort_select(&mut self, rank: usize) -> u32 {
        match self {
            Lats::Small(arr, n) => {
                let n = *n as usize;
                *arr[..n].select_nth_unstable(rank).1
            }
            Lats::Big(v) => *v.select_nth_unstable(rank).1,
        }
    }
}

const SHARD_BITS: usize = 10;
const SHARD_MASK: usize = (1 << SHARD_BITS) - 1;
const NO_NEXT: u32 = u32::MAX;

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

type KeyRow = (u32, u16, u32, u32, u32);

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

type Pending = (u64, u32, u8, usize, usize, u8); // key, lat, err, ep_off, ep_len, hour

fn flush_pending(pending: &mut Vec<Pending>, data: &[u8], shards: &Shards) {
    if pending.is_empty() {
        return;
    }
    // Counting sort by shard.
    let mut buckets: Vec<Vec<Pending>> = (0..1 << SHARD_BITS)
        .map(|_| Vec::new())
        .collect();
    for p in pending.drain(..) {
        buckets[(p.0 as usize) & SHARD_MASK].push(p);
    }
    for (shard_idx, bucket) in buckets.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let mut guard = shards[shard_idx].lock().expect("shard lock");
        let inner = &mut *guard;
        for p in bucket {
            let ep = &data[p.3..p.3 + p.4];
            match inner.map.entry(p.0) {
                Entry::Vacant(v) => {
                    let off = inner.arena.len() as u32;
                    inner.arena.extend_from_slice(ep);
                    inner.slots.push(Slot {
                        next: NO_NEXT,
                        off,
                        len: ep.len() as u32,
                        hour: p.5,
                        errors: p.2 as u32,
                        lats: Lats::new(p.1),
                    });
                    v.insert(inner.slots.len() as u32 - 1);
                }
                Entry::Occupied(mut o) => {
                    let mut sidx = *o.get();
                    loop {
                        let s = &mut inner.slots[sidx as usize];
                        if s.len as usize == ep.len()
                            && inner.arena[s.off as usize..s.off as usize + s.len as usize] == ep[..]
                        {
                            s.lats.push(p.1);
                            s.errors += p.2 as u32;
                            break;
                        }
                        if s.next == NO_NEXT {
                            let off = inner.arena.len() as u32;
                            inner.arena.extend_from_slice(ep);
                            let new_idx = inner.slots.len() as u32;
                            inner.slots.push(Slot {
                                next: *o.get(),
                                off,
                                len: ep.len() as u32,
                                hour: p.5,
                                errors: p.2 as u32,
                                lats: Lats::new(p.1),
                            });
                            *o.get_mut() = new_idx;
                            break;
                        }
                        sidx = s.next;
                    }
                }
            }
        }
    }
}

fn build_hour_table() -> Vec<String> {
    let mut v = Vec::with_capacity(72);
    for idx in 0..72u32 {
        v.push(match idx {
            0..=5 => format!("2026-10-30T{:02}:00:00Z", idx + 18),
            6..=29 => format!("2026-10-31T{:02}:00:00Z", idx - 6),
            30..=53 => format!("2026-11-01T{:02}:00:00Z", idx - 30),
            _ => format!("2026-11-02T{:02}:00:00Z", idx - 54),
        });
    }
    v
}

fn main() {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| "/out".to_string());
    let path: &'static str =
        Box::leak(format!("{data_dir}/requests.log").into_boxed_str());
    let out_path = format!("{out_dir}/summary.txt");

    let file = File::open(path).expect("open input");
    let len = file.metadata().expect("stat input").len() as usize;

    if len == 0 {
        File::create(&out_path).expect("create output");
        return;
    }

    #[cfg(unix)]
    let (data, _mmap_ptr, _mmap_len): (&[u8], *mut u8, usize) = {
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
        if ptr as usize == usize::MAX {
            panic!("mmap failed");
        }
        (unsafe { std::slice::from_raw_parts(ptr, len) }, ptr, len)
    };
    #[cfg(not(unix))]
    let (data, _mmap_ptr, _mmap_len): (&[u8], *mut u8, usize) = {
        let mut buf = vec![0u8; len];
        let mut f = file;
        let mut pos = 0usize;
        while pos < len {
            match <File as std::io::Read>::read(&mut f, &mut buf[pos..]) {
                Ok(0) => break,
                Ok(n) => pos += n,
                Err(e) => panic!("read: {e}"),
            }
        }
        let ptr = buf.as_ptr() as *mut u8;
        std::mem::forget(buf);
        (unsafe { std::slice::from_raw_parts(ptr, len) }, ptr, len)
    };

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

    std::thread::scope(|s| {
        let shards_ref = &shards;
        for i in 0..nthreads {
            let start = i * len / nthreads;
            let end = if i == nthreads - 1 { len } else { (i + 1) * len / nthreads };
            s.spawn(move || {
                let mut pos = start;
                if pos > 0 {
                    while pos < data.len() && data[pos - 1] != b'\n' {
                        pos += 1;
                    }
                }
                let mut pending: Vec<Pending> = Vec::with_capacity(4096);
                while pos < end {
                    let nl = match find_byte(data, pos, b'\n') {
                        Some(n) => n,
                        None => {
                            if pos < data.len() {
                                let line = &data[pos..];
                                let hour = hour_index(line);
                                let (m_end, e_end) = find_two_spaces(line, 28);
                                let ep = &line[m_end + 1..e_end];
                                let ep_off = ep.as_ptr() as usize - data.as_ptr() as usize;
                                let ep_len = ep.len();
                                let err5 = if line[e_end + 1] == b'5' { 1 } else { 0 };
                                let lat = parse_latency(&line[e_end + 5..]) as u32;
                                let h = fx_hash(ep);
                                let key = ((hour as u64) << 57) | (h & ((1 << 57) - 1));
                                pending.push((key, lat, err5, ep_off, ep_len, hour as u8));
                            }
                            break;
                        }
                    };
                    if pos >= nl {
                        pos = nl + 1;
                        continue;
                    }
                    let line = &data[pos..nl];
                    let hour = hour_index(line);
                    let (m_end, e_end) = find_two_spaces(line, 28);
                    let ep = &line[m_end + 1..e_end];
                    let ep_off = ep.as_ptr() as usize - data.as_ptr() as usize;
                    let ep_len = ep.len();
                    let err5 = if line[e_end + 1] == b'5' { 1 } else { 0 };
                    let lat = parse_latency(&line[e_end + 5..]) as u32;
                    let h = fx_hash(ep);
                    let key = ((hour as u64) << 57) | (h & ((1 << 57) - 1));
                    pending.push((key, lat, err5, ep_off, ep_len, hour as u8));
                    pos = nl + 1;
                    if pending.len() >= 4096 {
                        flush_pending(&mut pending, data, shards_ref);
                    }
                }
                flush_pending(&mut pending, data, shards_ref);
            });
        }
    });

    #[cfg(unix)]
    unsafe { munmap(_mmap_ptr, _mmap_len); }

    let mut inners: Vec<ShardInner> = shards
        .into_iter()
        .map(|m| m.into_inner().expect("shard inner"))
        .collect();

    let total_keys: usize = inners.iter().map(|i| i.slots.len()).sum();
    let mut sort_arena: Vec<u8> = Vec::with_capacity(1 << 19);
    let mut rows: Vec<KeyRow> = Vec::with_capacity(total_keys);
    for (si, inner) in inners.iter().enumerate() {
        for (k, slot) in inner.slots.iter().enumerate() {
            rows.push((slot.hour as u32, si as u16, k as u32, sort_arena.len() as u32, slot.len as u32));
            sort_arena.extend_from_slice(
                &inner.arena[slot.off as usize..slot.off as usize + slot.len as usize],
            );
        }
    }
    rows.sort_unstable_by(|a, b| {
        a.0
            .cmp(&b.0)
            .then_with(|| {
                sort_arena[a.3 as usize..a.3 as usize + a.4 as usize]
                    .cmp(&sort_arena[b.3 as usize..b.3 as usize + b.4 as usize])
            })
    });

    let hour_table = build_hour_table();

    let out = File::create(&out_path).expect("create output");
    let mut w = std::io::BufWriter::with_capacity(1 << 16, out);
    let mut buf: Vec<u8> = Vec::with_capacity(192);

    for k in &rows {
        let s = &mut inners[k.1 as usize].slots[k.2 as usize];
        let n = s.lats.len() as u64;
        let rank = |p: u64| -> usize { ((p * n + 99) / 100 - 1) as usize };
        let r99 = rank(99);
        let p99 = s.lats.sort_select(r99);
        let r95 = rank(95).min(r99);
        let p95 = if r95 == r99 { p99 } else { s.lats.sort_select(r95) };
        let r50 = rank(50).min(r95);
        let p50 = if r50 == r95 { p95 } else { s.lats.sort_select(r50) };
        let ppm = s.errors as u64 * 1_000_000 / n;

        buf.clear();
        buf.extend_from_slice(hour_table[k.0 as usize].as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(&sort_arena[k.3 as usize..k.3 as usize + k.4 as usize]);
        for v in [n, p50 as u64, p95 as u64, p99 as u64, s.errors as u64, ppm] {
            buf.push(b' ');
            push_int(&mut buf, v);
        }
        buf.push(b'\n');
        w.write_all(&buf).expect("write");
    }
    w.flush().expect("flush");
}
