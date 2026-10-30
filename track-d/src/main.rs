// Track D Step 4 — parallel queries + phase timings.
// Same bidirectional search as Step 3 (proven: output hash matched twice).
// Changes:
//  - queries run across threads with an atomic work-stealing counter, so a
//    hard query can't strand one thread while others idle. Results land in
//    per-slot atomics, then write out in query order.
//  - per-thread distance stores (sharing them across threads would be a
//    data race; each thread's store is its own). Stamps are u16: query
//    generations are <= 2000, and u16 stamps save 7 MB per store.
//  - thread count: THREADS env if set, else machine parallelism, else 12.
//  - phase timings on stderr: load vs queries, so the next step attacks
//    whichever phase actually dominates.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Instant;

const INF: u64 = u64::MAX;

// ---------------------------------------------------------------- input

struct Scanner {
    buf: Vec<u8>,
    pos: usize,
}

impl Scanner {
    fn next(&mut self) -> Option<u64> {
        let b = &self.buf;
        let mut i = self.pos;
        while i < b.len() && !b[i].is_ascii_digit() {
            i += 1;
        }
        if i >= b.len() {
            self.pos = i;
            return None;
        }
        let mut v: u64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            v = v * 10 + (b[i] - b'0') as u64;
            i += 1;
        }
        self.pos = i;
        Some(v)
    }
}

fn first_line_fields(buf: &[u8]) -> Vec<u64> {
    let end = buf.iter().position(|&c| c == b'\n').unwrap_or(buf.len());
    let mut fields = Vec::new();
    let mut v: u64 = 0;
    let mut in_num = false;
    for &c in &buf[..end] {
        if c.is_ascii_digit() {
            v = v * 10 + (c - b'0') as u64;
            in_num = true;
        } else if in_num {
            fields.push(v);
            v = 0;
            in_num = false;
        }
    }
    if in_num {
        fields.push(v);
    }
    fields
}

fn skip_first_line(sc: &mut Scanner) {
    sc.pos = sc
        .buf
        .iter()
        .position(|&c| c == b'\n')
        .map(|p| p + 1)
        .unwrap_or(sc.buf.len());
}

fn read_file(path: &str) -> Vec<u8> {
    let mut f = File::open(path).expect("open input");
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).expect("read input");
    buf
}

// ---------------------------------------------------------------- CSR

// (offsets[n+1], dest[m], weights[m]); offsets/dest/weights all u32.
type Csr = (Vec<u32>, Vec<u32>, Vec<u32>);

fn build_csr(n: usize, edges: &[(u32, u32, u32)], reverse: bool) -> Csr {
    let mut offsets = vec![0u32; n + 1];
    for &(u, v, _) in edges {
        let src = if reverse { v } else { u };
        offsets[src as usize + 1] += 1;
    }
    for i in 0..n {
        offsets[i + 1] += offsets[i];
    }
    let m = edges.len();
    let mut dest = vec![0u32; m];
    let mut wts = vec![0u32; m];
    let mut cur = offsets[..n].to_vec();
    for &(u, v, w) in edges {
        let (src, dst) = if reverse { (v, u) } else { (u, v) };
        let p = cur[src as usize] as usize;
        dest[p] = dst;
        wts[p] = w;
        cur[src as usize] += 1;
    }
    (offsets, dest, wts)
}

// Graph ids are used exactly as written, no base adjustment: graph and
// queries share the numbering, and n covers the largest id seen.
fn load_graph(path: &str) -> (usize, Csr, Csr) {
    let buf = read_file(path);
    let fields = first_line_fields(&buf);
    let mut sc = Scanner { buf, pos: 0 };
    let mut header_n: Option<usize> = None;
    if fields.len() == 2 {
        header_n = Some(fields[0] as usize);
        skip_first_line(&mut sc);
    } else if fields.len() != 3 {
        panic!("unexpected graph first line: {} fields", fields.len());
    }
    let mut edges: Vec<(u32, u32, u32)> = Vec::new();
    let mut max_id: u64 = 0;
    loop {
        let u = match sc.next() {
            Some(v) => v,
            None => break,
        };
        let v = sc.next().expect("edge v");
        let w = sc.next().expect("edge w");
        if u > max_id {
            max_id = u;
        }
        if v > max_id {
            max_id = v;
        }
        edges.push((u as u32, v as u32, w as u32));
    }
    let n = header_n.unwrap_or(0).max(max_id as usize + 1);
    let fwd = build_csr(n, &edges, false);
    let rev = build_csr(n, &edges, true);
    eprintln!(
        "graph: n={} edges={} header_n={:?} max_id={}",
        n,
        edges.len(),
        header_n,
        max_id
    );
    (n, fwd, rev)
}

fn load_queries(path: &str) -> Vec<(u32, u32)> {
    let buf = read_file(path);
    let fields = first_line_fields(&buf);
    let mut sc = Scanner { buf, pos: 0 };
    if fields.len() == 1 {
        skip_first_line(&mut sc);
    } else if fields.len() != 2 {
        panic!("unexpected queries first line: {} fields", fields.len());
    }
    let mut qs = Vec::new();
    loop {
        let s = match sc.next() {
            Some(v) => v,
            None => break,
        };
        let t = sc.next().expect("query t");
        qs.push((s as u32, t as u32));
    }
    eprintln!("queries: {}", qs.len());
    qs
}

// ---------------------------------------------------------------- solver

// Distance store with a per-query generation stamp, so a new query costs
// O(1) instead of an O(n) refill. Stamps are u16: generations are query
// indices + 1, at most 2000 here, far below the u16 limit.
struct Dist {
    d: Vec<u64>,
    seen: Vec<u16>,
    cur: u16,
}

impl Dist {
    fn new(n: usize) -> Self {
        Dist {
            d: vec![0; n],
            seen: vec![0; n],
            cur: 0,
        }
    }

    fn start(&mut self, gen: u16) {
        self.cur = gen;
    }

    #[inline]
    fn get(&self, node: usize) -> u64 {
        if self.seen[node] == self.cur {
            self.d[node]
        } else {
            INF
        }
    }

    #[inline]
    fn set(&mut self, node: usize, v: u64) {
        self.seen[node] = self.cur;
        self.d[node] = v;
    }
}

// Drop stale heap tops (entries whose node now has a smaller distance).
// Must run before reading a heap's top for the stop check.
fn clean_top(heap: &mut BinaryHeap<Reverse<(u64, u32)>>, dist: &Dist) {
    while let Some(&Reverse((d, u))) = heap.peek() {
        if d == dist.get(u as usize) {
            break;
        }
        heap.pop();
    }
}

fn bidir(
    fo: &[u32],
    fd: &[u32],
    fw: &[u32],
    ro: &[u32],
    rd: &[u32],
    rw: &[u32],
    s: u32,
    t: u32,
    df: &mut Dist,
    db: &mut Dist,
    gen: u16,
) -> i64 {
    if s == t {
        return 0;
    }
    df.start(gen);
    db.start(gen);
    let mut hf: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
    let mut hb: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::new();
    df.set(s as usize, 0);
    db.set(t as usize, 0);
    hf.push(Reverse((0, s)));
    hb.push(Reverse((0, t)));
    let mut mu: u64 = INF;
    loop {
        clean_top(&mut hf, df);
        clean_top(&mut hb, db);
        let top_f = hf.peek().map(|r| r.0.0).unwrap_or(INF);
        let top_b = hb.peek().map(|r| r.0.0).unwrap_or(INF);
        if top_f == INF && top_b == INF {
            break;
        }
        if mu != INF && top_f.saturating_add(top_b) >= mu {
            break;
        }
        if top_f <= top_b {
            // Expand forward: edge u -> v.
            let Reverse((d, u)) = hf.pop().unwrap();
            if db.get(u as usize) != INF {
                mu = mu.min(d + db.get(u as usize));
            }
            let start = fo[u as usize] as usize;
            let end = fo[u as usize + 1] as usize;
            for i in start..end {
                let nd = d + fw[i] as u64;
                let v = fd[i] as usize;
                if nd < df.get(v) {
                    df.set(v, nd);
                    hf.push(Reverse((nd, fd[i])));
                    let dbv = db.get(v);
                    if dbv != INF {
                        mu = mu.min(nd + dbv);
                    }
                }
            }
        } else {
            // Expand backward: reverse edge x -> y means original y -> x.
            let Reverse((d, x)) = hb.pop().unwrap();
            if df.get(x as usize) != INF {
                mu = mu.min(d + df.get(x as usize));
            }
            let start = ro[x as usize] as usize;
            let end = ro[x as usize + 1] as usize;
            for i in start..end {
                let nd = d + rw[i] as u64;
                let y = rd[i] as usize;
                if nd < db.get(y) {
                    db.set(y, nd);
                    hb.push(Reverse((nd, rd[i])));
                    let dfy = df.get(y);
                    if dfy != INF {
                        mu = mu.min(nd + dfy);
                    }
                }
            }
        }
    }
    if mu == INF {
        -1
    } else {
        mu as i64
    }
}

fn thread_count() -> usize {
    if let Ok(v) = std::env::var("THREADS") {
        if let Ok(n) = v.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(12)
}

fn main() {
    let t0 = Instant::now();
    let (n, fwd, rev) = load_graph("/data/graph.txt");
    let queries = load_queries("/data/queries.txt");
    let (fo, fd, fw) = fwd;
    let (ro, rd, rw) = rev;
    let qcount = queries.len();
    let t_load = t0.elapsed();

    let nthreads = thread_count();
    eprintln!("threads: {}", nthreads);
    let results: Vec<AtomicI64> = (0..qcount).map(|_| AtomicI64::new(0)).collect();
    let next = AtomicUsize::new(0);
    let t_q0 = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..nthreads {
            scope.spawn(|| {
                let mut df = Dist::new(n);
                let mut db = Dist::new(n);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= qcount {
                        break;
                    }
                    let (s, t) = queries[i];
                    let d = if (s as usize) >= n || (t as usize) >= n {
                        if s == t {
                            0
                        } else {
                            -1
                        }
                    } else {
                        bidir(
                            &fo, &fd, &fw, &ro, &rd, &rw, s, t, &mut df, &mut db,
                            (i + 1) as u16,
                        )
                    };
                    results[i].store(d, Ordering::Relaxed);
                }
            });
        }
    });
    let t_q = t_q0.elapsed();

    let out = File::create("/out/distances.txt").expect("create output");
    let mut w = BufWriter::new(out);
    for r in &results {
        writeln!(w, "{}", r.load(Ordering::Relaxed)).expect("write");
    }
    w.flush().expect("flush");
    eprintln!(
        "done: n={} queries={} load={:?} queries={:?} total={:?}",
        n,
        qcount,
        t_load,
        t_q,
        t0.elapsed()
    );
}
