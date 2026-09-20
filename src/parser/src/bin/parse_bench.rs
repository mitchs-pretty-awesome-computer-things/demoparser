//! Parse-speed benchmark harness for the parse-speedup project (see PARSE_SPEEDUP_GOAL.md).
//! Representative full parse (player props + events + ticks), timed wall-clock, ST and/or MT,
//! N iterations -> min / median / max. Baseline is fixed BEFORE any optimization.
//!
//! Usage: parse_bench <demo.dem> [iters=5] [mode=both|st|mt]

use parser::first_pass::parser_settings::{rm_user_friendly_names, ParserInputs};
use parser::parse_demo::{DemoOutput, Parser, ParsingMode};
use parser::second_pass::parser_settings::create_huffman_lookup_table;
use ahash::AHashMap;
use memmap2::MmapOptions;
use std::env;
use std::fs::File;
use std::time::Instant;

/// Representative workload: the props a typical stats/positions parse requests. Broad enough to
/// exercise the entity + prop-decode hot path without being a synthetic micro-case.
fn wanted_props() -> Vec<String> {
    [
        "tick", "health", "X", "Y", "Z",
        "velocity_X", "velocity_Y", "velocity_Z",
        "CCSPlayerPawn.m_angEyeAngles",
        "is_alive", "team_num", "active_weapon_name",
        "FORWARD", "LEFT", "RIGHT", "BACK", "FIRE", "is_walking", "is_airborne",
        "flash_duration", "armor_value", "balance",
    ]
    .iter().map(|s| s.to_string()).collect()
}

fn settings<'a>(huf: &'a Vec<(u8, u8)>) -> ParserInputs<'a> {
    // Engine constraint (collect_data.rs): tick columns are only collected
    // when the caller opts in via `collect_ticks`. Real usage needs two full
    // scans (parse_events then parse_ticks). CS2_TICKS=1 measures the
    // memory-heavy ticks workload; CS2_BOTH=1 proves the new unified scan
    // (ticks AND events in one pass); default measures decode+events.
    let unified = std::env::var("CS2_BOTH").is_ok();
    let ticks_only = std::env::var("CS2_TICKS").is_ok();
    let wanted_events = if ticks_only && !unified {
        vec![]
    } else {
        vec!["all".to_string()]
    };
    let collect_ticks = ticks_only || unified;
    settings_with(huf, wanted_events, collect_ticks)
}

fn settings_with<'a>(
    huf: &'a Vec<(u8, u8)>,
    wanted_events: Vec<String>,
    collect_ticks: bool,
) -> ParserInputs<'a> {
    let wanted = wanted_props();
    // Real bindings resolve friendly names (X, health, ...) to real sendtable
    // prop names via rm_user_friendly_names; without this the harness silently
    // collects 0 columns (see prop_controller matching on real names).
    let real = rm_user_friendly_names(&wanted).expect("friendly names");
    let mut og = AHashMap::default();
    for (r, f) in real.iter().zip(&wanted) {
        og.insert(r.clone(), f.clone());
    }
    ParserInputs {
        wanted_player_props: real.clone(),
        wanted_events,
        real_name_to_og_name: og,
        // NOTE: `other` props are team/game-state props, NOT player props.
        // Passing the player list here too makes dual-listed ids collect
        // twice into the same id-keyed column (misaligned 2× rows). Real
        // parse_ticks bindings pass [].
        wanted_other_props: vec![],
        parse_ents: true,
        wanted_players: vec![],
        wanted_ticks: vec![],
        parse_projectiles: false,
        parse_grenades: false,
        grenade_classes: None,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: huf,
        order_by_steamid: false,
        wanted_prop_states: AHashMap::default(),
        fallback_bytes: None,
        collect_ticks,
        parse_voice: false,
    }
}

fn run_once(mmap: &[u8], huf: &Vec<(u8, u8)>, mode: ParsingMode) -> f64 {
    let mut parser = Parser::new(settings(huf), mode);
    let t = Instant::now();
    let out = parser.parse_demo(mmap).expect("parse");
    let secs = t.elapsed().as_secs_f64();
    // touch output so the optimizer can't elide the parse
    std::hint::black_box(&out.df);
    // Deterministic golden checksum of the output DataFrame (CS2_CKSUM=1). DefaultHasher uses
    // fixed keys, so the hash is stable across runs/builds — used for before/after identity checks.
    if std::env::var("CS2_CKSUM").is_ok() {
        print_cksum(&out);
    }
    secs
}

fn print_cksum(out: &DemoOutput) {
    print_cksum_tagged(out, "cksum");
}

fn print_cksum_tagged(out: &DemoOutput, tag: &str) {
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let df: BTreeMap<_, _> = out.df.iter().collect();
    format!("{:?}", df).hash(&mut h);
    let dfp: BTreeMap<_, _> = out.df_per_player.iter().map(|(k, v)| (k, v.iter().collect::<BTreeMap<_, _>>())).collect();
    format!("{:?}", dfp).hash(&mut h);
    eprintln!("[{tag}] df={:016x} cols={} per_player={}", h.finish(), out.df.len(), out.df_per_player.len());
}

/// Streaming proof: preparse once, pull chunks lazily, merge at the end.
/// `df` must equal the MT parse bit-for-bit (same per-segment path).
fn run_once_chunked(mmap: &[u8], huf: &Vec<(u8, u8)>) -> f64 {
    let parser = Parser::new(settings(huf), ParsingMode::ForceMultiThreaded);
    let t = Instant::now();
    let shared = parser.preparse(mmap).expect("preparse");
    let n_segments = shared.fullpacket_offsets.len();
    let mut it = parser.iter_chunks(mmap, shared);
    let mut chunks = vec![];
    for c in &mut it {
        chunks.push(c.expect("chunk"));
    }
    let out = parser.combine_chunks(&mut chunks, it.shared());
    let secs = t.elapsed().as_secs_f64();
    std::hint::black_box(&out.df);
    if std::env::var("CS2_CKSUM").is_ok() {
        print_cksum(&out);
    }
    eprintln!("[chunks] segments={n_segments}");
    secs
}

/// Round (start, end) tick boundaries from one events scan.
fn round_bounds(mmap: &[u8], huf: &Vec<(u8, u8)>) -> Vec<(i32, i32)> {
    let mut ev_parser = Parser::new(
        settings_with(huf, vec!["all".to_string()], false),
        ParsingMode::ForceMultiThreaded,
    );
    let ev_out = ev_parser.parse_demo(mmap).expect("events");
    let mut evs: Vec<_> = ev_out
        .game_events
        .iter()
        .filter(|e| e.name == "round_start" || e.name == "round_end")
        .collect();
    evs.sort_by_key(|e| e.tick);
    let mut bounds: Vec<(i32, i32)> = vec![];
    let mut open: Option<i32> = None;
    for e in evs {
        if e.name == "round_start" {
            open = Some(e.tick);
        } else if let Some(lo) = open.take() {
            if e.tick > lo {
                bounds.push((lo, e.tick));
            }
        }
    }
    bounds
}

/// End-to-end "rounds" pattern: one events scan for round_start/round_end
/// boundaries, then one bounded `parse_tick_range` per round. Reports total
/// wall + rows, comparable against a full `parse_ticks` (CS2_TICKS=1).
fn run_once_pattern(mmap: &[u8], huf: &Vec<(u8, u8)>) -> f64 {
    use parser::first_pass::prop_controller::TICK_ID;
    use parser::second_pass::variants::VarVec;
    let t = Instant::now();
    // Phase 1: round boundaries (events only, no tick columns).
    let bounds = round_bounds(mmap, huf);
    // Phase 2: bounded range per round (tick columns only).
    let ticks_parser = Parser::new(settings_with(huf, vec![], true), ParsingMode::ForceMultiThreaded);
    let shared = ticks_parser.preparse(mmap).expect("preparse");
    let mut rows = 0usize;
    for (lo, hi) in &bounds {
        let out = ticks_parser
            .parse_tick_range(mmap, shared.clone(), *lo, *hi)
            .expect("range");
        if let Some(VarVec::I32(ticks)) = out.df.get(&TICK_ID).and_then(|c| c.data.as_ref()) {
            rows += ticks.iter().flatten().count();
        }
        std::hint::black_box(&out.df);
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!("[pattern] rounds={} rows={rows}", bounds.len());
    secs
}

/// Batched multi-range: same round bounds, but ONE `parse_tick_ranges` call
/// (each segment decoded once, event post-proc once). Must yield the same
/// rows as the per-range pattern with less wall.
fn run_once_ranges(mmap: &[u8], huf: &Vec<(u8, u8)>) -> f64 {
    use parser::first_pass::prop_controller::TICK_ID;
    use parser::second_pass::variants::VarVec;
    let t = Instant::now();
    let bounds = round_bounds(mmap, huf);
    let ticks_parser = Parser::new(settings_with(huf, vec![], true), ParsingMode::ForceMultiThreaded);
    let shared = ticks_parser.preparse(mmap).expect("preparse");
    // CS2_BATCH=k: grouped batching dial (k ranges per batch; default: all).
    let batch: usize = std::env::var("CS2_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let outs = if batch == usize::MAX {
        ticks_parser
            .parse_tick_ranges(mmap, shared, &bounds)
            .expect("ranges")
    } else {
        ticks_parser
            .parse_tick_ranges_grouped(mmap, shared, &bounds, batch)
            .expect("ranges")
    };
    let mut rows = 0usize;
    for (i, out) in outs.iter().enumerate() {
        if let Some(VarVec::I32(ticks)) = out.df.get(&TICK_ID).and_then(|c| c.data.as_ref()) {
            let n = ticks.iter().flatten().count();
            rows += n;
            let mn = ticks.iter().flatten().min();
            let mx = ticks.iter().flatten().max();
            eprintln!("[rangerows] {i}: rows={n} min={mn:?} max={mx:?}");
        }
        std::hint::black_box(&out.df);
        if std::env::var("CS2_CKSUM").is_ok() {
            print_cksum_tagged(out, &format!("range{i}"));
        }
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!("[ranges] rounds={} rows={rows}", bounds.len());
    for (i, (lo, hi)) in bounds.iter().enumerate() {
        eprintln!("[bounds] {i}: {lo},{hi}");
    }
    secs
}

/// Tick-range proof: parse only overlapping segments, verify every emitted
/// row/event tick falls inside `[lo, hi]`.
fn run_once_range(mmap: &[u8], huf: &Vec<(u8, u8)>, lo: i32, hi: i32) -> f64 {
    use parser::first_pass::prop_controller::TICK_ID;
    use parser::second_pass::variants::VarVec;
    let parser = Parser::new(settings(huf), ParsingMode::ForceMultiThreaded);
    let t = Instant::now();
    let shared = parser.preparse(mmap).expect("preparse");
    let out = parser
        .parse_tick_range(mmap, shared, lo, hi)
        .expect("range");
    let secs = t.elapsed().as_secs_f64();
    std::hint::black_box(&out.df);
    let (mut mn, mut mx, mut n) = (i32::MAX, i32::MIN, 0usize);
    if let Some(col) = out.df.get(&TICK_ID).and_then(|c| c.data.as_ref()) {
        if let VarVec::I32(ticks) = col {
            for t in ticks.iter().flatten() {
                mn = mn.min(*t);
                mx = mx.max(*t);
                n += 1;
            }
        }
    }
    let ev_ok = out.game_events.iter().all(|e| e.tick >= lo && e.tick <= hi);
    eprintln!("[range] rows={n} tick_min={mn} tick_max={mx} events_ok={ev_ok}");
    if std::env::var("CS2_CKSUM").is_ok() {
        print_cksum(&out);
    }
    secs
}

// ParsingMode is not Copy; take a constructor so each run gets a fresh value without touching the lib.
fn bench(mmap: &[u8], huf: &Vec<(u8, u8)>, make_mode: impl Fn() -> ParsingMode, label: &str, iters: usize) {
    // one warm-up (page cache / branch predictors) excluded from stats
    let _ = run_once(mmap, huf, make_mode());
    let mut times: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        times.push(run_once(mmap, huf, make_mode()));
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = times[0];
    let med = times[times.len() / 2];
    let max = *times.last().unwrap();
    println!("{label:>4}: min {min:.3}s  median {med:.3}s  max {max:.3}s  (n={iters})");
}

fn main() {
    let demo_path = env::args().nth(1).expect("usage: parse_bench <demo.dem> [iters] [st|mt|both]");
    let iters: usize = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mode = env::args().nth(3).unwrap_or_else(|| "both".to_string());

    let huf = create_huffman_lookup_table();
    let file = File::open(&demo_path).expect("open demo");
    let mmap = unsafe { MmapOptions::new().map(&file).unwrap() };
    println!("demo: {demo_path}  ({:.1} MB)", mmap.len() as f64 / 1e6);

    // Streaming/range proofs (Step 1c). CS2_CHUNKS: chunked parse must match
    // MT bit-for-bit. CS2_RANGE="lo,hi": bounded tick-range parse.
    // CS2_PATTERN=1: events scan for round boundaries + one range per round.
    if std::env::var("CS2_CHUNKS").is_ok() {
        let _ = run_once_chunked(&mmap, &huf);
        let mut times = vec![];
        for _ in 0..iters {
            times.push(run_once_chunked(&mmap, &huf));
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("CHUNK: min {:.3}s  median {:.3}s  max {:.3}s  (n={iters})", times[0], times[times.len() / 2], times[times.len() - 1]);
        return;
    }
    if let Ok(range) = std::env::var("CS2_RANGE") {
        let (lo, hi) = range.split_once(',').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).expect("CS2_RANGE=lo,hi");
        let _ = run_once_range(&mmap, &huf, lo, hi);
        let mut times = vec![];
        for _ in 0..iters {
            times.push(run_once_range(&mmap, &huf, lo, hi));
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("RANGE[{lo},{hi}]: min {:.3}s  median {:.3}s  max {:.3}s  (n={iters})", times[0], times[times.len() / 2], times[times.len() - 1]);
        return;
    }
    if std::env::var("CS2_PATTERN").is_ok() {
        let _ = run_once_pattern(&mmap, &huf);
        let mut times = vec![];
        for _ in 0..iters {
            times.push(run_once_pattern(&mmap, &huf));
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("PATTERN: min {:.3}s  median {:.3}s  max {:.3}s  (n={iters})", times[0], times[times.len() / 2], times[times.len() - 1]);
        return;
    }
    if std::env::var("CS2_RANGES").is_ok() {
        let _ = run_once_ranges(&mmap, &huf);
        let mut times = vec![];
        for _ in 0..iters {
            times.push(run_once_ranges(&mmap, &huf));
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("RANGES: min {:.3}s  median {:.3}s  max {:.3}s  (n={iters})", times[0], times[times.len() / 2], times[times.len() - 1]);
        return;
    }

    if mode == "st" || mode == "both" {
        bench(&mmap, &huf, || ParsingMode::ForceSingleThreaded, "ST", iters);
    }
    if mode == "mt" || mode == "both" {
        bench(&mmap, &huf, || ParsingMode::ForceMultiThreaded, "MT", iters);
    }
}
