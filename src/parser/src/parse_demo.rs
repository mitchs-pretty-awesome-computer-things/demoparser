use crate::first_pass::frameparser::{FrameParser, StartEndOffset, StartEndType};
use crate::first_pass::parser::SharedFirstPass;
use crate::first_pass::parser_settings::check_multithreadability;
use crate::first_pass::parser_settings::continuous_decode_required;
use crate::first_pass::parser_settings::{FirstPassParser, ParserInputs};
use crate::first_pass::prop_controller::{PropController, NAME_ID, STEAMID_ID, TICK_ID};
use crate::first_pass::read_bits::DemoParserError;
use crate::second_pass::collect_data::ProjectileRecord;
use crate::second_pass::game_events::{EventField, GameEvent};
use crate::second_pass::parser::SecondPassOutput;
use crate::second_pass::parser_settings::*;
use crate::second_pass::variants::VarVec;
use crate::second_pass::variants::{PropColumn, Variant};
use ahash::AHashMap;
use ahash::AHashSet;
use csgoproto::CsvcMsgVoiceData;
use itertools::Itertools;
use rayon::iter::IntoParallelIterator;
use rayon::iter::IntoParallelRefIterator;
use rayon::prelude::ParallelIterator;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::Duration;

pub const HEADER_ENDS_AT_BYTE: usize = 16;

#[derive(Debug)]
pub struct DemoOutput {
    pub df: AHashMap<u32, PropColumn>,
    pub game_events: Vec<GameEvent>,
    pub skins: Vec<EconItem>,
    pub item_drops: Vec<EconItem>,
    pub chat_messages: Vec<ChatMessageRecord>,
    pub convars: AHashMap<String, String>,
    pub header: Option<AHashMap<String, String>>,
    pub player_md: Vec<PlayerEndMetaData>,
    /// Live player roster from CCSPlayerController entities (final per-player state,
    /// deduplicated by steamid). Populated even when the end-of-match scoreboard message
    /// is absent (community/casual demos). Fallback when `player_md` is empty.
    pub roster: Vec<PlayerEndMetaData>,
    pub game_events_counter: AHashSet<String>,
    pub uniq_prop_names: Vec<String>,
    pub projectiles: Vec<ProjectileRecord>,
    pub voice_data: Vec<(i32, CsvcMsgVoiceData)>,
    pub prop_controller: PropController,
    pub df_per_player: AHashMap<u64, AHashMap<u32, PropColumn>>,
}

pub struct Parser<'a> {
    input: ParserInputs<'a>,
    pub parsing_mode: ParsingMode,
}

/// Lazily parsed fullpacket segment. See `Parser::iter_chunks`.
pub struct ChunkIter<'a, 'b> {
    shared: SharedFirstPass,
    inputs: &'b ParserInputs<'a>,
    bytes: &'b [u8],
    next_idx: usize,
}

impl<'a, 'b> ChunkIter<'a, 'b> {
    /// The shared preparse backing this iteration (for `combine_chunks`).
    pub fn shared(&self) -> &SharedFirstPass {
        &self.shared
    }
}

impl<'a, 'b> Iterator for ChunkIter<'a, 'b> {    type Item = Result<SecondPassOutput, DemoParserError>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = *self.shared.fullpacket_offsets.get(self.next_idx)?;
        self.next_idx += 1;
        let res = (|| {
            let mut parser = SecondPassParser::new(&self.shared, self.inputs, offset, false, None)?;
            parser.start(self.bytes)?;
            Ok(parser.create_output())
        })();
        Some(res)
    }
}

/// Indices of fullpacket segments overlapping tick range `[lo, hi]`.
/// Segment `i` covers ticks `[tick_i, tick_{i+1})`; the last covers EOF.
/// `fullpacket_ticks` is ascending (see `into_shared`), so this binary-searches.
pub fn segments_for_ticks(shared: &SharedFirstPass, lo: i32, hi: i32) -> std::ops::Range<usize> {
    let ticks = &shared.fullpacket_ticks;
    if ticks.is_empty() {
        return 0..0;
    }
    // Last segment whose snapshot tick is <= lo (saturates to 0).
    let start = ticks.partition_point(|&t| t <= lo).saturating_sub(1);
    // One past the last segment whose snapshot tick is <= hi; always ahead of start.
    let end = ticks.partition_point(|&t| t <= hi).max(start + 1);
    start..end.min(ticks.len())
}

/// `segments_for_ticks` plus a one-segment pre-roll: range decoding starts
/// one snapshot early so rows demuxed around the first in-range fullpacket
/// are not missed. When `lo` equals a fullpacket tick, tick `lo`'s ordinary
/// DemPacket is demuxed BEFORE that fullpacket and belongs to the preceding
/// segment — the raw overlap selection would drop part of tick `lo`'s rows
/// (the full parse decodes every segment and keeps both parts). The pre-roll
/// is unconditional rather than gated on `lo` landing on a snapshot tick so
/// coverage does not depend on demux packet ordering; the extra segment only
/// ever contributes out-of-range rows, which are trimmed (singular) or routed
/// away (plural).
fn segments_for_tick_range(shared: &SharedFirstPass, lo: i32, hi: i32) -> std::ops::Range<usize> {
    let range = segments_for_ticks(shared, lo, hi);
    if range.start > 0 { (range.start - 1)..range.end } else { range }
}

/// Range-agnostic per-segment items routed to overlapping ranges by segment.
#[derive(Default, Clone)]
struct MiscBucket {
    skins: Vec<EconItem>,
    item_drops: Vec<EconItem>,
    chat_messages: Vec<ChatMessageRecord>,
    convars: AHashMap<String, String>,
    player_md: Vec<PlayerEndMetaData>,
    roster: Vec<PlayerEndMetaData>,
    projectiles: Vec<ProjectileRecord>,
    voice_data: Vec<(i32, CsvcMsgVoiceData)>,
    game_events_counter: AHashSet<String>,
    uniq_prop_names: Vec<String>,
}

/// Materialize `None`-only bucket columns to the variant seen elsewhere in
/// the SAME segment (any range). `push`-built columns stay lazy
/// (`data: None`) while `slice_to_new` materializes; without this, a prop
/// that is null throughout one range but valued in a neighboring range of
/// the same segment would hash/serialize differently between routed and
/// single-range outputs despite identical values. Residual edge: a prop
/// valued only outside ALL ranges degrades to a dropped (vs null) column,
/// matching full-parse representation for that range.
///
/// SoA-only residual (documented, not fixed): row (AoS) outputs always
/// carry every requested key — `soa_to_aos` emits untyped nulls without
/// needing a type — but `structOfArrays` serialization omits columns with
/// no typed values, so a prop valued only outside the decoded ranges is
/// absent from a bounded SoA map while the full parse emits a null-filled
/// column. This was investigated and deliberately left: a prop id's
/// `Variant` type cannot be derived cleanly from sendtable metadata.
/// `PropInfo` carries only `PropType` (Player/Weapon/Custom/...), and the
/// column type is decided post-decode in the second pass (`find_prop`
/// retypes several props — spotted masks `U32`→`U64Vec`, colors `I32`→
/// `String`, coordinates computed as `F32` regardless of decoder — while
/// custom-derived props like velocity/inventory/stickers never pass
/// through a single decoder at all). A registry would hand-map every one
/// of those paths and silently mistype columns on any drift, which is
/// worse than an omitted column; bounded SoA outputs keep the narrower
/// behavior.
fn normalize_segment_buckets(rb: &mut RangedBuckets) {
    let mut templates: AHashMap<u32, Variant> = AHashMap::default();
    for m in rb.dfs.iter() {
        harvest_templates(m, &mut templates);
    }
    for pmap in rb.per_player.iter() {
        for m in pmap.values() {
            harvest_templates(m, &mut templates);
        }
    }
    if templates.is_empty() {
        return;
    }
    for m in rb.dfs.iter_mut() {
        materialize_nones(m, &templates);
    }
    for pmap in rb.per_player.iter_mut() {
        for m in pmap.values_mut() {
            materialize_nones(m, &templates);
        }
    }
}

/// Dummy `Variant` matching a materialized `VarVec`'s type, used to build
/// an all-`None` column of the same type (see `empty_like`). Shared with
/// `normalize_segment_buckets` so all materialization agrees on the
/// representation.
fn dummy_for(data: &VarVec) -> Variant {
    match data {
        VarVec::Bool(_) => Variant::Bool(false),
        VarVec::I32(_) => Variant::I32(0),
        VarVec::F32(_) => Variant::F32(0.0),
        VarVec::String(_) => Variant::String(String::new()),
        VarVec::U64(_) => Variant::U64(0),
        VarVec::U32(_) => Variant::U32(0),
        VarVec::StringVec(_) => Variant::StringVec(vec![]),
        VarVec::U64Vec(_) => Variant::U64Vec(vec![]),
        VarVec::U32Vec(_) => Variant::U32Vec(vec![]),
        VarVec::XYVec(_) => Variant::VecXY([0.0, 0.0]),
        VarVec::XYZVec(_) => Variant::VecXYZ([0.0, 0.0, 0.0]),
        VarVec::Stickers(_) => Variant::Stickers(vec![]),
        VarVec::InputHistory(_) => Variant::InputHistory(vec![]),
        VarVec::UserCmdSubtickMoves(_) => Variant::UserCmdSubtickMoves(vec![]),
    }
}

fn empty_like(dummy: &Variant, n: usize) -> VarVec {
    let mut vv = VarVec::new(dummy);
    for _ in 0..n {
        vv.push_variant(None);
    }
    vv
}

/// Per-prop-id variant template from every materialized column in `map`.
/// First materialized column wins (a prop id always decodes to one type).
fn harvest_templates(map: &AHashMap<u32, PropColumn>, templates: &mut AHashMap<u32, Variant>) {
    for (id, col) in map.iter() {
        if let Some(data) = col.data.as_ref() {
            templates.entry(*id).or_insert_with(|| dummy_for(data));
        }
    }
}

fn materialize_nones(m: &mut AHashMap<u32, PropColumn>, templates: &AHashMap<u32, Variant>) {
    for (id, col) in m.iter_mut() {
        if col.data.is_none() {
            if let Some(d) = templates.get(id) {
                col.data = Some(empty_like(d, col.num_nones));
                col.num_nones = 0;
            }
        }
    }
}

/// Tick span covered by segment `seg_idx`: `[tick_i, tick_{i+1})`, last runs
/// through EOF. Used to assign range-agnostic items (skins, chat, voice) to
/// overlapping ranges.
fn segment_tick_span(shared: &SharedFirstPass, seg_idx: usize) -> (i32, i32) {
    let ticks = &shared.fullpacket_ticks;
    let lo = ticks.get(seg_idx).copied().unwrap_or(i32::MIN);
    let hi = ticks.get(seg_idx + 1).copied().unwrap_or(i32::MAX);
    (lo, hi)
}

/// Drop `df` / `df_per_player` rows and game events outside `[lo, hi]`.
/// Boundary segments decode out-of-range ticks (needed for entity state),
/// so the merged result is trimmed after. Range-agnostic metadata (skins,
/// drops, chat, convars, header, rosters, projectiles, voice) is kept whole.
fn trim_to_tick_range(out: &mut DemoOutput, lo: i32, hi: i32) {
    fn indices_in_range(df: &AHashMap<u32, PropColumn>, lo: i32, hi: i32) -> Option<Vec<usize>> {
        match df.get(&TICK_ID).and_then(|c| c.data.as_ref()) {
            Some(VarVec::I32(ticks)) => Some(
                ticks
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, t)| match t {
                        Some(t) if *t >= lo && *t <= hi => Some(idx),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }
    if let Some(wanted) = indices_in_range(&out.df, lo, hi) {
        let mut trimmed = AHashMap::default();
        for (k, v) in out.df.iter() {
            if let Some(new) = v.slice_to_new(&wanted) {
                trimmed.insert(*k, new);
            }
        }
        out.df = trimmed;
    }
    // Always trim per-player maps on their OWN tick column. With
    // `order_by_steamid` rows live in `df_per_player` while `out.df` has no
    // tick column, so gating this loop on `out.df`'s trim would skip it and
    // return every tick of the boundary segments untrimmed.
    for (_, per_player) in out.df_per_player.iter_mut() {
        if let Some(w) = indices_in_range(per_player, lo, hi) {
            let mut t = AHashMap::default();
            for (k, v) in per_player.iter() {
                if let Some(new) = v.slice_to_new(&w) {
                    t.insert(*k, new);
                }
            }
            *per_player = t;
        }
    }
    out.game_events.retain(|e| e.tick >= lo && e.tick <= hi);
}
#[derive(PartialEq)]
pub enum ParsingMode {
    ForceSingleThreaded,
    ForceMultiThreaded,
    Normal,
}

impl<'a> Parser<'a> {
    pub fn new(input: ParserInputs<'a>, parsing_mode: ParsingMode) -> Self {
        Parser {
            input: input,
            parsing_mode: parsing_mode,
        }
    }
    /// First pass only: sendtables/classinfo/baselines/header plus the
    /// fullpacket segment index. The returned snapshot is owned and cheap to
    /// share by reference across chunk iteration and rayon threads.
    pub fn preparse(&self, demo_bytes: &[u8]) -> Result<SharedFirstPass, DemoParserError> {
        FirstPassParser::new(&self.input).parse_demo(demo_bytes, false)
    }
    /// Lazily parse one `SecondPassOutput` per fullpacket segment, in file
    /// order. Each chunk is self-contained (snapshot init at its fullpacket),
    /// so callers can process and drop chunks one at a time to bound peak
    /// memory instead of merging the whole demo via `combine_chunks`.
    ///
    /// Rejects props that require a continuous decode (cross-segment state
    /// or velocity-derived props): per-segment decode cannot carry their
    /// state, so streaming them would return silently-reset values. Use
    /// `parse_tick_range` or the full `parse_demo` for those props.
    pub fn iter_chunks<'b>(&'b self, demo_bytes: &'b [u8], shared: SharedFirstPass) -> Result<ChunkIter<'a, 'b>, DemoParserError> {
        self.iter_chunks_from(demo_bytes, shared, 0)
    }
    /// Same as `iter_chunks` but resumes at segment `start_idx` (an index
    /// into `shared.fullpacket_offsets`) instead of 0. The WASM `ChunkParser`
    /// handle persists its cursor across `nextChunk` calls and resumes here,
    /// so each segment is decoded exactly once. An out-of-range `start_idx`
    /// simply yields an empty iterator.
    pub fn iter_chunks_from<'b>(
        &'b self,
        demo_bytes: &'b [u8],
        shared: SharedFirstPass,
        start_idx: usize,
    ) -> Result<ChunkIter<'a, 'b>, DemoParserError> {
        let offending = continuous_decode_required(&self.input.wanted_player_props);
        if !offending.is_empty() {
            return Err(DemoParserError::NonMultithreadableProps(format!(
                "iter_chunks cannot serve props {:?} that require cross-segment state or continuous decode (stateful props or velocity-derived props): per-segment decode cannot carry their state across segments; use parse_tick_range or parse_demo instead",
                offending
            )));
        }
        Ok(ChunkIter {
            shared,
            inputs: &self.input,
            bytes: demo_bytes,
            next_idx: start_idx,
        })
    }
    /// Parse only the tick range `[lo, hi]` and drop rows/events outside
    /// it. Two decode strategies, chosen by prop kind:
    ///
    /// - Per-segment-safe props: only the overlapping segments (plus a
    ///   one-segment pre-roll for boundary-tick coverage) are decoded.
    ///   Segment decode is byte-identical to the full parse by construction
    ///   (same per-segment path); out-of-range rows from the boundary
    ///   segments are dropped after merging.
    /// - Props that require a continuous decode (see
    ///   `continuous_decode_required`: cross-segment state such as damage
    ///   counters, plus velocity-derived props): the DEMO PREFIX is decoded
    ///   continuously from `HEADER_ENDS_AT_BYTE` up to the end of the last
    ///   selected segment. Stateful props accumulate across the whole demo
    ///   and a fullpacket snapshot does not restore them, so starting at the
    ///   range would begin with reset state and diverge from the full
    ///   single-threaded parse. Cost is ~ the full single-threaded parse up
    ///   to the range end; that is inherent to correctness, not overhead —
    ///   only per-segment-safe props get the bounded decode.
    ///
    /// Unlike the per-segment surfaces (`parse_tick_ranges`, `iter_chunks`,
    /// `ChunkParser`), this serves continuous-decode props via the prefix
    /// fallback (`parse_tick_range_continuous`), so velocity and stateful
    /// props here match the full one-shot parse.
    ///
    /// SoA-only caveat (see `normalize_segment_buckets`): row outputs
    /// carry every requested key as nulls when never valued in range, but
    /// `structOfArrays` serialization omits such never-valued columns while
    /// the full parse emits null-filled ones.
    pub fn parse_tick_range(
        &self,
        demo_bytes: &[u8],
        shared: SharedFirstPass,
        lo: i32,
        hi: i32,
    ) -> Result<DemoOutput, DemoParserError> {
        // `continuous_decode_required` is exactly the union of the two old
        // conditions (`!check_multithreadability` for NON_MULTITHREADABLE
        // state props, `needs_velocity` for `contains("velo")` props), so
        // this branch serves the same queries the old fallback did.
        if !continuous_decode_required(&self.input.wanted_player_props).is_empty() {
            return self.parse_tick_range_continuous(demo_bytes, shared, lo, hi);
        }
        let sel_range = segments_for_tick_range(&shared, lo, hi);
        let selected: Vec<usize> = shared.fullpacket_offsets[sel_range.clone()].to_vec();
        let mut outputs: Vec<SecondPassOutput> = {
            // WASM has no threads: rayon would have nowhere to run, so parse
            // segments serially there. Native keeps the parallel segment parse.
            #[cfg(target_arch = "wasm32")]
            let mut outputs: Vec<SecondPassOutput> = {
                let mut v = Vec::with_capacity(selected.len());
                for offset in &selected {
                    let mut parser = SecondPassParser::new(&shared, &self.input, *offset, false, None)?;
                    parser.start(demo_bytes)?;
                    v.push(parser.create_output());
                }
                v
            };
            #[cfg(not(target_arch = "wasm32"))]
            let mut outputs: Vec<SecondPassOutput> = selected
                .par_iter()
                .map(|offset| {
                    let mut parser = SecondPassParser::new(&shared, &self.input, *offset, false, None)?;
                    parser.start(demo_bytes)?;
                    Ok(parser.create_output())
                })
                .collect::<Result<Vec<_>, DemoParserError>>()?;
            outputs
        };
        let mut out = self.combine_chunks(&mut outputs, &shared);
        Parser::remove_duplicate_player_connects(&mut out.game_events);
        Parser::add_item_purchase_sell_column(&mut out.game_events);
        Parser::remove_item_sold_events(&mut out.game_events);
        trim_to_tick_range(&mut out, lo, hi);
        Ok(out)
    }
    /// Continuous fallback for props that require a demo-prefix decode
    /// (cross-segment state and velocity-derived props): one parser runs
    /// from the DEMO START (`HEADER_ENDS_AT_BYTE`, mirroring
    /// `second_pass_single_threaded`'s `SecondPassParser::new(&shared,
    /// &self.input, 16, true, None)`) to the end of the last selected
    /// segment (the next fullpacket offset, or EOF).
    ///
    /// The byte-16 start takes the no-seed path in
    /// `parse_full_packet_and_break_if_needed` (`start ==
    /// HEADER_ENDS_AT_BYTE` skips snapshot seeding): signon frames already
    /// built that state live, exactly like the continuous full parse, so the
    /// seed-once delta handling composes without change.
    ///
    /// MEMORY: decoding the prefix without suppression would accumulate the
    /// whole prefix in `df`, defeating the bounded API. Prefix rows are
    /// suppressed during decode with range routing
    /// (`set_range_routing(vec![(lo, hi)])`): rows outside the range
    /// early-return in `collect_entities_ranged` while entity state AND
    /// velocity history still advance, so the in-range slice matches the
    /// full parse. Buckets are drained with `take_range_buckets()` and
    /// assembled via the shared `assemble_ranged_output` (no duplicated
    /// assembly logic with `parse_tick_ranges_inner`).
    fn parse_tick_range_continuous(
        &self,
        demo_bytes: &[u8],
        shared: SharedFirstPass,
        lo: i32,
        hi: i32,
    ) -> Result<DemoOutput, DemoParserError> {
        let seg_range = segments_for_ticks(&shared, lo, hi);
        let end = shared
            .fullpacket_offsets
            .get(seg_range.end)
            .copied()
            .unwrap_or(demo_bytes.len());
        let bound = StartEndOffset {
            start: HEADER_ENDS_AT_BYTE,
            end,
            msg_type: StartEndType::OK,
        };
        let mut parser = SecondPassParser::new(&shared, &self.input, HEADER_ENDS_AT_BYTE, false, Some(bound))?;
        parser.set_range_routing(vec![(lo, hi)]);
        parser.start(demo_bytes)?;
        let mut buckets = parser.take_range_buckets().unwrap_or_default();
        let mut out = parser.create_output();
        // Same lazy-None normalization as the multi-range path (no-op for a
        // single in-range bucket, kept so both paths agree by construction).
        normalize_segment_buckets(&mut buckets);
        // The single bucket's maps arrive in `bounds` order; with one range
        // that is just `dfs[0]` / `per_player[0]`. Moves only (rows were
        // pushed into buckets during decode).
        let mut df_parts = Vec::new();
        if let Some(dfmap) = buckets.dfs.into_iter().next() {
            if !dfmap.is_empty() {
                df_parts.push(dfmap);
            }
        }
        let mut pp_parts: AHashMap<u64, Vec<AHashMap<u32, PropColumn>>> = AHashMap::default();
        if let Some(pmap) = buckets.per_player.into_iter().next() {
            for (steamid, pmap) in pmap {
                if !pmap.is_empty() {
                    pp_parts.entry(steamid).or_default().push(pmap);
                }
            }
        }
        // Defensive: row collection normally routes everything into buckets,
        // leaving `out.df`/`out.df_per_player` empty, but the projectile path
        // (`collect_projectiles`) bypasses routing and pushes straight into
        // `out.df`. Carry any such rows along; the `trim_to_tick_range` below
        // bounds them to `[lo, hi]`.
        if !out.df.is_empty() {
            df_parts.push(std::mem::take(&mut out.df));
        }
        for (steamid, pmap) in std::mem::take(&mut out.df_per_player) {
            if !pmap.is_empty() {
                pp_parts.entry(steamid).or_default().push(pmap);
            }
        }
        // Event post-processing mirrors the full parse, then the in-range
        // slice is retained so the prefix events decoded for state do not leak.
        Parser::remove_duplicate_player_connects(&mut out.game_events);
        Parser::add_item_purchase_sell_column(&mut out.game_events);
        Parser::remove_item_sold_events(&mut out.game_events);
        out.game_events.retain(|e| e.tick >= lo && e.tick <= hi);
        // Tick-carrying misc is filtered to the range. The remaining misc
        // (skins, item drops, chat, convars, end-of-match/player metadata,
        // live roster, event-name set, seen-prop list) carries no tick and is
        // range-agnostic: with a byte-16 start it spans the whole prefix, a
        // SUPERSET of what the old segment-bounded fallback returned. Kept
        // whole to match `trim_to_tick_range` semantics (which likewise keeps
        // misc whole); bindings ignore these fields entirely.
        out.projectiles
            .retain(|p| p.tick.map(|t| t >= lo && t <= hi).unwrap_or(true));
        out.voice_data.retain(|(tick, _)| *tick >= lo && *tick <= hi);
        let misc = MiscBucket {
            skins: std::mem::take(&mut out.skins),
            item_drops: std::mem::take(&mut out.item_drops),
            chat_messages: std::mem::take(&mut out.chat_messages),
            convars: std::mem::take(&mut out.convars),
            player_md: std::mem::take(&mut out.player_md),
            roster: std::mem::take(&mut out.roster),
            projectiles: std::mem::take(&mut out.projectiles),
            voice_data: std::mem::take(&mut out.voice_data),
            game_events_counter: std::mem::take(&mut out.game_events_counter),
            uniq_prop_names: std::mem::take(&mut out.uniq_prop_names).into_iter().collect(),
        };
        let mut assembled =
            self.assemble_ranged_output(&shared, df_parts, pp_parts, misc, std::mem::take(&mut out.game_events));
        // Backstop on the same tested path as every other bounded surface:
        // no-op when routing suppressed correctly, bounds anything the
        // defensive carry-along above may have contributed.
        trim_to_tick_range(&mut assembled, lo, hi);
        Ok(assembled)
    }
    /// Parse many tick ranges in ONE pass: every overlapping segment is
    /// decoded exactly once, rows/events are routed to range buckets, and
    /// event post-processing (dedup, purchase/sell) runs ONCE globally.
    /// Fixes the N× decode + N× post-proc cost of calling `parse_tick_range`
    /// per range. Rows in overlapping ranges go to the first range (sorted by
    /// `lo`); rows outside all ranges are dropped. Returned in input order.
    ///
    /// Rejects props that require a continuous decode (cross-segment state
    /// or velocity-derived props): each segment is decoded exactly once
    /// with rows routed per range, so that state cannot be preserved. Use
    /// `parse_tick_range` (continuous fallback) or the full `parse_demo`
    /// for those props.
    pub fn parse_tick_ranges(
        &self,
        demo_bytes: &[u8],
        shared: SharedFirstPass,
        ranges: &[(i32, i32)],
    ) -> Result<Vec<DemoOutput>, DemoParserError> {
        self.parse_tick_ranges_inner(demo_bytes, &shared, ranges)
    }

    /// Grouped batching: the wall/memory dial between per-range (slow,
    /// lean) and full-batch (fast, fat). Ranges are split into groups of
    /// `batch_size`; each group decodes its union once and post-processes
    /// once. Event post-proc is per-group, so cross-group sell matching
    /// falls back to range-local semantics at group edges (documented).
    ///
    /// Like `parse_tick_ranges`, rejects props that require a continuous
    /// decode (cross-segment state or velocity-derived props).
    pub fn parse_tick_ranges_grouped(
        &self,
        demo_bytes: &[u8],
        shared: SharedFirstPass,
        ranges: &[(i32, i32)],
        batch_size: usize,
    ) -> Result<Vec<DemoOutput>, DemoParserError> {
        let mut out = Vec::with_capacity(ranges.len());
        for chunk in ranges.chunks(batch_size.max(1)) {
            out.extend(self.parse_tick_ranges_inner(demo_bytes, &shared, chunk)?);
        }
        Ok(out)
    }

    fn parse_tick_ranges_inner(
        &self,
        demo_bytes: &[u8],
        shared: &SharedFirstPass,
        ranges: &[(i32, i32)],
    ) -> Result<Vec<DemoOutput>, DemoParserError> {
        if ranges.is_empty() {
            return Ok(vec![]);
        }
        // Props that require a continuous decode (cross-fullpacket state,
        // velocity-derived props) cannot be served here: this path decodes
        // each segment exactly once with rows routed per range, so that
        // state cannot be preserved. Fail loudly instead of returning
        // silently-wrong rows; callers needing those props should use
        // `parse_tick_range` (continuous fallback) or the full `parse_demo`.
        let offending = continuous_decode_required(&self.input.wanted_player_props);
        if !offending.is_empty() {
            return Err(DemoParserError::NonMultithreadableProps(format!(
                "parse_tick_ranges cannot serve props {:?} that require cross-segment state or continuous decode (stateful props or velocity-derived props): per-segment decode cannot carry their state across segments; use parse_tick_range or parse_demo instead",
                offending
            )));
        }
        // Deterministic assignment: sort by lo, first match wins.
        let mut sorted: Vec<(i32, i32, usize)> = ranges
            .iter()
            .enumerate()
            .map(|(i, &(lo, hi))| (lo, hi, i))
            .collect();
        sorted.sort_by_key(|&(lo, _, _)| lo);
        let n = ranges.len();

        // Union of segments overlapping any range; each decoded ONCE with rows
        // routed straight into per-range buckets (no post-partition copy).
        let batch_bounds: Vec<(i32, i32)> =
            sorted.iter().map(|&(lo, hi, _)| (lo, hi)).collect();
        let mut seg_set = std::collections::BTreeSet::new();
        for &(lo, hi, _) in &sorted {
            seg_set.extend(segments_for_tick_range(shared, lo, hi));
        }
        let seg_list: Vec<usize> = seg_set.into_iter().collect();
        let mut decoded: Vec<(usize, RangedBuckets, SecondPassOutput)> = seg_list
            .par_iter()
            .map(|&seg_idx| {
                let offset = shared.fullpacket_offsets[seg_idx];
                let mut parser = SecondPassParser::new(shared, &self.input, offset, false, None)?;
                parser.set_range_routing(batch_bounds.clone());
                parser.start(demo_bytes)?;
                let buckets = parser.take_range_buckets().unwrap_or_default();
                Ok((seg_idx, buckets, parser.create_output()))
            })
            .collect::<Result<Vec<_>, DemoParserError>>()?;
        decoded.sort_by_key(|(_, _, out)| out.ptr);

        // Per-range buckets (indexed by ORIGINAL range order).
        let mut df_buckets: Vec<Vec<AHashMap<u32, PropColumn>>> = vec![Vec::new(); n];
        let mut pp_buckets: Vec<AHashMap<u64, Vec<AHashMap<u32, PropColumn>>>> =
            vec![AHashMap::default(); n];
        let mut events_all: Vec<GameEvent> = Vec::new();
        let mut misc: Vec<MiscBucket> = vec![MiscBucket::default(); n];

        for (seg_idx, mut rb, out) in decoded {
            // Normalize lazy-None columns before routing (see fn docs).
            normalize_segment_buckets(&mut rb);
            let span = segment_tick_span(&shared, seg_idx);
            // Ranges overlapping this segment (for range-agnostic items).
            let overlapping: Vec<usize> = sorted
                .iter()
                .filter(|&&(lo, hi, _)| lo < span.1 && hi >= span.0)
                .map(|&(_, _, orig)| orig)
                .collect();
            // Routed buckets arrive in `sorted` order; re-index to ORIGINAL.
            // Moves only (rows were pushed into buckets during decode).
            for (si, dfmap) in rb.dfs.into_iter().enumerate() {
                if !dfmap.is_empty() {
                    df_buckets[sorted[si].2].push(dfmap);
                }
            }
            for (si, pmap) in rb.per_player.into_iter().enumerate() {
                let orig = sorted[si].2;
                for (steamid, pmap) in pmap {
                    if !pmap.is_empty() {
                        pp_buckets[orig].entry(steamid).or_default().push(pmap);
                    }
                }
            }
            let mut out = out;
            events_all.extend(out.game_events.drain(..));
            for &orig in &overlapping {
                let m = &mut misc[orig];
                m.skins.extend(out.skins.iter().cloned());
                m.item_drops.extend(out.item_drops.iter().cloned());
                m.chat_messages.extend(out.chat_messages.iter().cloned());
                m.convars.extend(out.convars.iter().map(|(k, v)| (k.clone(), v.clone())));
                m.player_md.extend(out.player_md.iter().cloned());
                m.roster.extend(out.roster.iter().cloned());
                m.projectiles.extend(out.projectiles.iter().cloned());
                m.voice_data.extend(out.voice_data.iter().cloned());
                for e in out.game_events_counter.iter() {
                    m.game_events_counter.insert(e.clone());
                }
                m.uniq_prop_names.extend(out.uniq_prop_names.iter().cloned());
            }
        }

        // Event post-processing ONCE globally, then split per range.
        let mut events_all_mut = events_all;
        Parser::remove_duplicate_player_connects(&mut events_all_mut);
        Parser::add_item_purchase_sell_column(&mut events_all_mut);
        Parser::remove_item_sold_events(&mut events_all_mut);
        let mut events_per_range: Vec<Vec<GameEvent>> = vec![Vec::new(); n];
        for e in events_all_mut {
            for &(lo, hi, orig) in &sorted {
                if e.tick < lo {
                    break;
                }
                if e.tick <= hi {
                    events_per_range[orig].push(e);
                    break;
                }
            }
        }

        // Assemble one DemoOutput per range (input order). SoA-only
        // residual (see `normalize_segment_buckets`): row outputs carry
        // every requested key, but `structOfArrays` serialization omits
        // columns never valued in the decoded segments.
        let mut results: Vec<DemoOutput> = Vec::with_capacity(n);
        for orig in 0..n {
            results.push(self.assemble_ranged_output(
                shared,
                std::mem::take(&mut df_buckets[orig]),
                std::mem::take(&mut pp_buckets[orig]),
                std::mem::take(&mut misc[orig]),
                std::mem::take(&mut events_per_range[orig]),
            ));
        }
        Ok(results)
    }
    /// Shared bucket-to-output assembly for the ranged surfaces: combine one
    /// range's routed row parts, attach its range-agnostic items and its
    /// already-post-processed in-range events. Used by
    /// `parse_tick_ranges_inner` (per range) and the singular
    /// `parse_tick_range_continuous` so the two cannot drift apart.
    fn assemble_ranged_output(
        &self,
        shared: &SharedFirstPass,
        df_parts: Vec<AHashMap<u32, PropColumn>>,
        pp_parts: AHashMap<u64, Vec<AHashMap<u32, PropColumn>>>,
        misc: MiscBucket,
        events: Vec<GameEvent>,
    ) -> DemoOutput {
        let mut prop_controller = shared.prop_controller.clone();
        for prop in &shared.added_temp_props {
            prop_controller.wanted_player_props.retain(|x| x != prop);
            prop_controller.prop_infos.retain(|x| &x.prop_name != prop);
        }
        let df = self.combine_dfs(df_parts, false);
        let mut pp = AHashMap::default();
        for (steamid, v) in pp_parts {
            pp.insert(steamid, self.combine_dfs(v, true));
        }
        let mut roster_by_sid: std::collections::BTreeMap<u64, PlayerEndMetaData> =
            std::collections::BTreeMap::new();
        for p in &misc.roster {
            if let Some(sid) = p.steamid {
                if sid != 0 {
                    roster_by_sid.insert(sid, p.clone());
                }
            }
        }
        let mut all_prop_names: Vec<String> = misc.uniq_prop_names.iter().cloned().collect();
        all_prop_names.sort();
        all_prop_names.dedup();
        DemoOutput {
            prop_controller,
            chat_messages: misc.chat_messages,
            item_drops: misc.item_drops,
            player_md: misc.player_md,
            roster: roster_by_sid.into_values().collect(),
            game_events: events,
            skins: misc.skins,
            convars: misc.convars,
            df,
            header: Some(shared.header.clone()),
            game_events_counter: misc.game_events_counter,
            projectiles: misc.projectiles,
            voice_data: misc.voice_data,
            df_per_player: pp,
            uniq_prop_names: all_prop_names,
        }
    }
    /// Merge per-segment outputs into one `DemoOutput` (the multi-threaded
    /// tail: per-prop parallel concat). Prefer streaming chunks via
    /// `iter_chunks` when peak memory matters; this materialises everything.
    pub fn combine_chunks(
        &self,
        second_pass_outputs: &mut Vec<SecondPassOutput>,
        shared: &SharedFirstPass,
    ) -> DemoOutput {
        let mut out = self.combine_outputs(second_pass_outputs, shared);
        if let Some(new_df) = self.rm_unwanted_ticks(&mut out.df) {
            out.df = new_df;
        }
        out
    }
    pub fn parse_demo(&mut self, demo_bytes: &[u8]) -> Result<DemoOutput, DemoParserError> {
        let _prof = std::env::var("CS2_PROF").is_ok();
        // Instant::now() traps on wasm: only construct the clock when profiling.
        let _t = _prof.then(std::time::Instant::now);
        let shared = self.preparse(demo_bytes)?;
        if let Some(_t) = _t.as_ref() {
            eprintln!("[prof] first_pass: {:.3}s", _t.elapsed().as_secs_f64());
        }
        if self.parsing_mode == ParsingMode::Normal
            && check_multithreadability(&self.input.wanted_player_props)
            && !(self.parsing_mode == ParsingMode::ForceSingleThreaded)
            || self.parsing_mode == ParsingMode::ForceMultiThreaded
        {
            return self.second_pass_multi_threaded(demo_bytes, shared);
        } else {
            self.second_pass_single_threaded(demo_bytes, shared)
        }
    }

    fn second_pass_multi_threaded(&self, outer_bytes: &[u8], shared: SharedFirstPass) -> Result<DemoOutput, DemoParserError> {
        let second_pass_outputs: Vec<Result<SecondPassOutput, DemoParserError>> = shared
            .fullpacket_offsets
            .par_iter()
            .map(|offset| {
                let mut parser = SecondPassParser::new(&shared, &self.input, *offset, false, None)?;
                parser.start(outer_bytes)?;
                Ok(parser.create_output())
            })
            .collect();
        // check for errors
        let mut ok = vec![];
        for result in second_pass_outputs {
            match result {
                Err(e) => return Err(e),
                Ok(r) => ok.push(r),
            };
        }
        let mut outputs = self.combine_chunks(&mut ok, &shared);
        Parser::remove_duplicate_player_connects(&mut outputs.game_events);
        Parser::add_item_purchase_sell_column(&mut outputs.game_events);
        Parser::remove_item_sold_events(&mut outputs.game_events);
        Ok(outputs)
    }
    fn remove_duplicate_player_connects(events: &mut Vec<GameEvent>){
        let mut v = events.iter().filter(|x| x.name == "player_first_connect").collect_vec();
        v.sort_by_key(|x| x.tick);
        let mut ids = AHashMap::default();
        for x in v{
            for f in &x.fields{
                if f.name == "steamid"{
                    if let Some(Variant::U64(s)) = f.data{
                        match ids.get(&s) {
                            Some(_) => {},
                            None => {
                                ids.insert(s, x.clone());
                            }
                        }
                    }
                    }
                }
            }
        events.retain(|x|x.name != "player_first_connect");
        events.extend(ids.values().map(|x| x.clone()));
    }
    fn second_pass_single_threaded(&self, outer_bytes: &[u8], shared: SharedFirstPass) -> Result<DemoOutput, DemoParserError> {
        let prof = std::env::var("CS2_PROF").is_ok();
        // Same as above: Some exactly when profiling.
        let mut t = prof.then(std::time::Instant::now);
        let mut parser = SecondPassParser::new(&shared, &self.input, 16, true, None)?;
        parser.start(outer_bytes)?;
        if let Some(t0) = t {
            eprintln!("[prof] second_pass start(): {:.3}s", t0.elapsed().as_secs_f64());
            t = prof.then(std::time::Instant::now);
        }
        let second_pass_output = parser.create_output();
        if let Some(t0) = t {
            eprintln!("[prof] create_output: {:.3}s", t0.elapsed().as_secs_f64());
            t = prof.then(std::time::Instant::now);
        }
        let mut outputs = self.combine_chunks(&mut vec![second_pass_output], &shared);
        if let Some(t0) = t {
            eprintln!("[prof] combine_outputs: {:.3}s", t0.elapsed().as_secs_f64());
            t = prof.then(std::time::Instant::now);
        }
        Parser::add_item_purchase_sell_column(&mut outputs.game_events);
        Parser::remove_item_sold_events(&mut outputs.game_events);
        if let Some(t0) = t.as_ref() {
            eprintln!("[prof] post-proc: {:.3}s", t0.elapsed().as_secs_f64());
        }
        Ok(outputs)
    }
    fn second_pass_threaded_with_channels(
        &self,
        outer_bytes: &[u8],
        shared: &SharedFirstPass,
        reciever: Receiver<StartEndOffset>,
    ) -> Result<DemoOutput, DemoParserError> {
        thread::scope(|s| {
            let mut handles = vec![];
            let mut channel_threading_was_ok = true;
            let input = &self.input;
            let shared_ref = shared;
            loop {
                if let Ok(start_end_offset) = reciever.recv_timeout(Duration::from_secs(3)) {
                    match start_end_offset.msg_type {
                        StartEndType::EndOfMessages => break,
                        StartEndType::OK => {}
                        StartEndType::MultithreadingWasNotOk => {
                            channel_threading_was_ok = false;
                            break;
                        }
                    }
                    handles.push(s.spawn(move || {
                        let mut parser = SecondPassParser::new(shared_ref, input, start_end_offset.start, false, Some(start_end_offset))?;
                        parser.start(outer_bytes)?;
                        Ok(parser.create_output())
                    }));
                } else {
                    channel_threading_was_ok = false;
                    break;
                }
            }
            // Fallback if channels failed to find all fullpackets. Should be rare.
            if !channel_threading_was_ok {
                let shared = FirstPassParser::new(&self.input).parse_demo(outer_bytes, false)?;
                return self.second_pass_multi_threaded_no_channels(outer_bytes, shared);
            }
            // check for errors
            let mut ok = vec![];
            for result in handles {
                match result.join() {
                    Err(_e) => return Err(DemoParserError::MalformedMessage),
                    Ok(r) => {
                        ok.push(r?);
                    }
                };
            }
        let mut outputs = self.combine_chunks(&mut ok, shared);
            Parser::add_item_purchase_sell_column(&mut outputs.game_events);
            Parser::remove_item_sold_events(&mut outputs.game_events);
            return Ok(outputs);
        })
    }
    fn second_pass_multi_threaded_no_channels(&self, outer_bytes: &[u8], shared: SharedFirstPass) -> Result<DemoOutput, DemoParserError> {
        let second_pass_outputs: Vec<Result<SecondPassOutput, DemoParserError>> = shared
            .fullpacket_offsets
            .par_iter()
            .map(|offset| {
                let mut parser = SecondPassParser::new(&shared, &self.input, *offset, false, None)?;
                parser.start(outer_bytes)?;
                Ok(parser.create_output())
            })
            .collect();
        // check for errors
        let mut ok = vec![];
        for result in second_pass_outputs {
            match result {
                Err(e) => return Err(e),
                Ok(r) => ok.push(r),
            };
        }
        let mut outputs = self.combine_chunks(&mut ok, &shared);
        Parser::add_item_purchase_sell_column(&mut outputs.game_events);
        Parser::remove_item_sold_events(&mut outputs.game_events);
        Ok(outputs)
    }
    fn remove_item_sold_events(events: &mut Vec<GameEvent>) {
        events.retain(|x| x.name != "item_sold")
    }
    fn add_item_purchase_sell_column(events: &mut Vec<GameEvent>) {
        // Checks each item_purchase event for if the item was eventually sold

        let purchases = events.iter().filter(|x| x.name == "item_purchase").collect_vec();
        let sells = events.iter().filter(|x| x.name == "item_sold").collect_vec();

        let purchases = purchases.iter().filter_map(|event| SellBackHelper::from_event(event)).collect_vec();
        let sells = sells.iter().filter_map(|event| SellBackHelper::from_event(event)).collect_vec();

        let mut was_sold = vec![];
        for purchase in &purchases {
            let wanted_sells = sells
                .iter()
                .filter(|sell| sell.tick > purchase.tick && sell.steamid == purchase.steamid && sell.inventory_slot == purchase.inventory_slot);
            let wanted_buys = purchases
                .iter()
                .filter(|buy| buy.tick > purchase.tick && buy.steamid == purchase.steamid && buy.inventory_slot == purchase.inventory_slot);
            let min_tick_sells = wanted_sells.min_by_key(|x| x.tick);
            let min_tick_buys = wanted_buys.min_by_key(|x| x.tick);
            if let (Some(sell_tick), Some(buy_tick)) = (min_tick_sells, min_tick_buys) {
                if sell_tick.tick < buy_tick.tick {
                    was_sold.push(true);
                } else {
                    was_sold.push(false);
                }
            } else {
                was_sold.push(false);
            }
        }
        let mut idx = 0;
        for event in events {
            if event.name == "item_purchase" {
                event.fields.push(EventField {
                    name: "was_sold".to_string(),
                    data: Some(Variant::Bool(was_sold[idx])),
                });
                idx += 1;
            }
        }
    }
    fn rm_unwanted_ticks(&self, hm: &mut AHashMap<u32, PropColumn>) -> Option<AHashMap<u32, PropColumn>> {
        // Used for removing ticks when velocity is needed
        if self.input.wanted_ticks.is_empty() {
            return None;
        }
        let mut wanted_indicies = vec![];
        if let Some(ticks) = hm.get(&TICK_ID) {
            if let Some(VarVec::I32(t)) = &ticks.data {
                for (idx, val) in t.iter().enumerate() {
                    if let Some(tick) = val {
                        if self.input.wanted_ticks.contains(tick) {
                            wanted_indicies.push(idx);
                        }
                    }
                }
            }
        }
        let mut new_df = AHashMap::default();
        for (k, v) in hm {
            if let Some(new) = v.slice_to_new(&wanted_indicies) {
                new_df.insert(*k, new);
            }
        }
        Some(new_df)
    }

    fn combine_outputs(&self, second_pass_outputs: &mut Vec<SecondPassOutput>, shared: &SharedFirstPass) -> DemoOutput {
        // Combines all inner DemoOutputs into one big output
        let mut outputs = std::mem::take(second_pass_outputs);
        outputs.sort_by_key(|x| x.ptr);

        if outputs.len() == 1 {
            let output = outputs.pop().unwrap();
            let mut prop_controller = shared.prop_controller.clone();
            for prop in &shared.added_temp_props {
                prop_controller.wanted_player_props.retain(|x| x != prop);
                prop_controller.prop_infos.retain(|x| &x.prop_name != prop);
            }

            let mut pp = AHashMap::default();
            for (steamid, mut df) in output.df_per_player {
                df.remove(&STEAMID_ID);
                df.remove(&NAME_ID);
                pp.insert(steamid, df);
            }

            let mut all_prop_names: Vec<String> = output.uniq_prop_names.into_iter().collect();
            all_prop_names.sort();
            all_prop_names.dedup();

            let roster = {
                let mut by_sid: std::collections::BTreeMap<u64, PlayerEndMetaData> = std::collections::BTreeMap::new();
                for p in output.roster {
                    if let Some(sid) = p.steamid {
                        if sid != 0 {
                            by_sid.insert(sid, p);
                        }
                    }
                }
                by_sid.into_values().collect()
            };

            return DemoOutput {
                prop_controller,
                chat_messages: output.chat_messages,
                item_drops: output.item_drops,
                player_md: output.player_md,
                roster,
                game_events: output.game_events,
                skins: output.skins,
                convars: output.convars,
                df: output.df,
                header: Some(shared.header.clone()),
                game_events_counter: output.game_events_counter,
                projectiles: output.projectiles,
                voice_data: output.voice_data,
                df_per_player: pp,
                uniq_prop_names: all_prop_names,
            };
        }

        let mut dfs = Vec::with_capacity(outputs.len());
        let mut per_players: AHashMap<u64, Vec<AHashMap<u32, PropColumn>>> = AHashMap::default();
        let mut all_game_events = AHashSet::default();
        let mut all_prop_names = Vec::new();
        let mut chat_messages = Vec::new();
        let mut item_drops = Vec::new();
        let mut player_md = Vec::new();
        let mut roster_by_sid: std::collections::BTreeMap<u64, PlayerEndMetaData> = std::collections::BTreeMap::new();
        let mut game_events = Vec::new();
        let mut skins = Vec::new();
        let mut convars = AHashMap::default();
        let mut projectiles = Vec::new();
        let mut voice_data = Vec::new();

        for output in outputs {
            dfs.push(output.df);
            for event_name in output.game_events_counter {
                all_game_events.insert(event_name);
            }
            all_prop_names.extend(output.uniq_prop_names);
            for (steamid, df) in output.df_per_player {
                per_players.entry(steamid).or_default().push(df);
            }
            chat_messages.extend(output.chat_messages);
            item_drops.extend(output.item_drops);
            player_md.extend(output.player_md);
            for p in output.roster {
                if let Some(sid) = p.steamid {
                    if sid != 0 {
                        roster_by_sid.insert(sid, p);
                    }
                }
            }
            game_events.extend(output.game_events);
            skins.extend(output.skins);
            convars.extend(output.convars);
            projectiles.extend(output.projectiles);
            voice_data.extend(output.voice_data);
        }

        let all_dfs_combined = self.combine_dfs(dfs, false);
        all_prop_names.sort();
        all_prop_names.dedup();
        // Remove temp props
        let mut prop_controller = shared.prop_controller.clone();
        for prop in &shared.added_temp_props {
            prop_controller.wanted_player_props.retain(|x| x != prop);
            prop_controller.prop_infos.retain(|x| &x.prop_name != prop);
        }
        let mut pp = AHashMap::default();
        for (steamid, v) in per_players {
            let combined = self.combine_dfs(v, true);
            pp.insert(steamid, combined);
        }

        DemoOutput {
            prop_controller: prop_controller,
            chat_messages,
            item_drops,
            player_md,
            // Second-pass segments are sorted by ascending tick. Each captures a player's state
            // at its last tick; dedup by steamid keeping the LAST entry -> final name/team.
            roster: roster_by_sid.into_values().collect(),
            game_events,
            skins,
            convars,
            df: all_dfs_combined,
            header: Some(shared.header.clone()),
            game_events_counter: all_game_events,
            projectiles,
            voice_data,
            df_per_player: pp,
            uniq_prop_names: all_prop_names,
        }
    }

    fn combine_dfs(&self, mut v: Vec<AHashMap<u32, PropColumn>>, remove_name_and_steamid: bool) -> AHashMap<u32, PropColumn> {
        let mut big: AHashMap<u32, PropColumn> = AHashMap::default();
        if v.len() == 1 {
            let mut result = v.remove(0);
            if remove_name_and_steamid {
                result.remove(&STEAMID_ID);
                result.remove(&NAME_ID);
            }
            return result;
        }

        // Pre-group each chunk's columns into per-prop ordered buckets. This only MOVES the
        // PropColumn structs (no row-data copy) and preserves chunk (offset) order, so the
        // first bucket entry is the seed and the rest are appended in order — identical to the
        // serial insert/extend_from it replaces.
        let mut groups: AHashMap<u32, Vec<PropColumn>> = AHashMap::default();
        for part_df in v {
            for (k, col) in part_df {
                if remove_name_and_steamid && (k == STEAMID_ID || k == NAME_ID) {
                    continue;
                }
                groups.entry(k).or_default().push(col);
            }
        }
        // Concatenate each prop's segments in parallel. Columns are independent and the per-prop
        // order is preserved, so the result is byte-identical to the serial merge. This is the
        // dominant serial cost in the multi-threaded path (~25% of MT wall-clock on large demos).
        let groups_vec: Vec<(u32, Vec<PropColumn>)> = groups.into_iter().collect();
        let combined: Vec<(u32, PropColumn)> = groups_vec
            .into_par_iter()
            .map(|(k, mut segs)| {
                let mut acc = segs.remove(0);
                for mut seg in segs {
                    acc.extend_from(&mut seg);
                }
                (k, acc)
            })
            .collect();
        big.extend(combined);
        big
    }
}

#[derive(Debug)]
pub struct SellBackHelper {
    pub tick: i32,
    pub steamid: u64,
    pub inventory_slot: u32,
}
impl SellBackHelper {
    pub fn from_event(event: &GameEvent) -> Option<Self> {
        if let Some(Variant::I32(tick)) = SellBackHelper::extract_field("tick", &event.fields) {
            if let Some(Variant::U64(steamid)) = SellBackHelper::extract_field("steamid", &event.fields) {
                if let Some(Variant::U32(slot)) = SellBackHelper::extract_field("inventory_slot", &event.fields) {
                    return Some(SellBackHelper {
                        tick: tick,
                        steamid: steamid,
                        inventory_slot: slot,
                    });
                }
            }
        }
        None
    }
    fn extract_field(name: &str, fields: &[EventField]) -> Option<Variant> {
        for field in fields {
            if field.name == name {
                return field.data.clone();
            }
        }
        None
    }
}
