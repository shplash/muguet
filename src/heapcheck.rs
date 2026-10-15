// muguet - heapcheck.rs
// Constantin Clerc (c22dev) - 0.1

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GM: u64 = 0x1C3_AC30;
const GM_TOPSIZE: u64 = 16;
const GM_TOP: u64 = 40;
const GM_SEG_BASE: u64 = 928;
const GM_SEG_SIZE: u64 = 936;

const PINUSE: u64 = 1;
const CINUSE: u64 = 2;
const MIN_CHUNK: u64 = 32;

static ENABLED: AtomicBool = AtomicBool::new(false);
static JOURNAL: Mutex<VecDeque<(u64, u64, &'static str, Instant)>> = Mutex::new(VecDeque::new());
static WALKS: AtomicU64 = AtomicU64::new(0);
const JOURNAL_MAX: usize = 400_000;
const JOURNAL_AGE: Duration = Duration::from_secs(60);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn note(addr: u64, len: u64, what: &'static str) {
    if !enabled() { return; }
    let mut j = JOURNAL.lock().unwrap();
    if j.len() >= JOURNAL_MAX { j.pop_front(); }
    j.push_back((addr, len, what, Instant::now()));
}

pub fn start() {
    if std::env::var_os("MUGUET_HEAPCHECK").is_none() { return; }
    ENABLED.store(true, Ordering::Relaxed);
    eprintln!("[heapcheck] enabled");
    std::thread::Builder::new().name("heapcheck".into()).spawn(run).expect("spawn heapcheck");
}

fn mem() -> Option<(*mut u8, usize)> {
    crate::RT.get().map(|rt| crate::mem_base_len(&rt.mem))
}

fn rd(base: *mut u8, len: usize, addr: u64) -> Option<u64> {
    let a = addr as usize;
    if a + 8 > len { return None; }
    Some(unsafe { std::ptr::read_volatile(base.add(a) as *const u64) })
}

fn run() {
    let mut reported = false;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let Some((base, len)) = mem() else { continue };
        let t = Instant::now();
        let mut result = walk(base, len);
        if let Err(first) = &result {
            std::thread::sleep(Duration::from_millis(200));
            result = match walk(base, len) {
                Err(b) if b.chunk == first.chunk => Err(b),
                _ => { eprintln!("[heapcheck] transient mismatch at 0x{:x} ({}) gone on recheck", first.chunk, first.what); Ok(0) }
            };
        }
        let n = WALKS.fetch_add(1, Ordering::Relaxed) + 1;
        match result {
            Ok(chunks) => {
                if n % 30 == 1 { eprintln!("[heapcheck] heap ok: {chunks} chunks checked in {:?} (walk {n})", t.elapsed()); }
            }
            Err(bad) if !reported => {
                reported = true;
                report(base, len, &bad);
            }
            Err(_) => {}
        }
    }
}

struct Bad { what: String, chunk: u64, prev: u64, prev_size: u64 }

fn walk(base: *mut u8, len: usize) -> Result<u64, Bad> {
    let g = |o: u64| rd(base, len, GM + o).unwrap_or(0);
    let (top, topsize, seg_base, seg_size) = (g(GM_TOP), g(GM_TOPSIZE), g(GM_SEG_BASE), g(GM_SEG_SIZE));
    if top == 0 || seg_base == 0 { return Ok(0); }
    let seg_end = seg_base + seg_size;
    let mut q = seg_base + ((8 - ((seg_base + 16) & 7)) & 7);
    let (mut prev, mut prev_size, mut prev_inuse, mut n) = (0u64, 0u64, true, 0u64);
    let bad = |what: String, chunk: u64, prev: u64, prev_size: u64| Err(Bad { what, chunk, prev, prev_size });
    while q != top {
        if q < seg_base || q + 16 > seg_end { return bad(format!("chunk walk left the heap segment (top 0x{top:x})"), q, prev, prev_size); }
        let head = rd(base, len, q + 8).unwrap_or(0);
        let size = head & !7;
        if size < MIN_CHUNK { return bad(format!("bad chunk size {size} (head 0x{head:x})"), q, prev, prev_size); }
        if q + size > top { return bad(format!("chunk of size {size} runs past the top chunk at 0x{top:x}"), q, prev, prev_size); }
        if (head & PINUSE != 0) != prev_inuse { return bad(format!("previous-in-use flag {} does not match the previous chunk (in use {prev_inuse})", head & PINUSE), q, prev, prev_size); }
        let inuse = head & CINUSE != 0;
        if !inuse {
            if !prev_inuse { return bad("two free chunks next to each other".into(), q, prev, prev_size); }
            let foot = rd(base, len, q + size).unwrap_or(0);
            if foot != size { return bad(format!("free chunk footer {foot} does not match its size {size}"), q, prev, prev_size); }
        }
        prev = q;
        prev_size = size;
        prev_inuse = inuse;
        q += size;
        n += 1;
    }
    let th = rd(base, len, top + 8).unwrap_or(0);
    if th & !7 != topsize { return bad(format!("top chunk size {} does not match topsize {topsize}", th & !7), top, prev, prev_size); }
    if (th & PINUSE != 0) != prev_inuse { return bad("top chunk previous-in-use flag is wrong".into(), top, prev, prev_size); }
    Ok(n)
}

fn report(base: *mut u8, len: usize, bad: &Bad) {
    eprintln!("[heapcheck] HEAP CORRUPTION: {} at chunk 0x{:x}; previous chunk 0x{:x} (size {})", bad.what, bad.chunk, bad.prev, bad.prev_size);
    let dump = |from: u64, n: u64| {
        let words: Vec<String> = (0..n).map(|k| format!("{:016x}", rd(base, len, from + 8 * k).unwrap_or(0))).collect();
        eprintln!("[heapcheck]   0x{from:x}: {}", words.join(" "));
    };
    if bad.prev != 0 { dump(bad.prev, 4); dump(bad.prev + bad.prev_size.saturating_sub(32), 4); }
    dump(bad.chunk, 6);
    let lo = if bad.prev != 0 { bad.prev } else { bad.chunk.saturating_sub(256) };
    let hi = bad.chunk + 256;
    let now = Instant::now();
    let j = JOURNAL.lock().unwrap();
    let hits: Vec<_> = j.iter().filter(|(a, l, _, t)| *a < hi && a + l.max(&1) > lo && now.duration_since(*t) < JOURNAL_AGE).collect();
    eprintln!("[heapcheck] muguet writes into 0x{lo:x}..0x{hi:x} in the last {}s: {} (journal holds {})", JOURNAL_AGE.as_secs(), hits.len(), j.len());
    for (a, l, what, t) in hits.iter().rev().take(40) {
        eprintln!("[heapcheck]   {what}: 0x{a:x} +{l} ({:.1}s ago)", now.duration_since(*t).as_secs_f64());
    }
}
