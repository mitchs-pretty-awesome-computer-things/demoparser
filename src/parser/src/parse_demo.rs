use crate::first_pass::frameparser::{FrameParser, StartEndOffset, StartEndType};
use crate::first_pass::parser::SharedFirstPass;
use crate::first_pass::parser_settings::check_multithreadability;
use crate::first_pass::parser_settings::{FirstPassParser, ParserInputs};
use crate::first_pass::prop_controller::{PropController, NAME_ID, STEAMID_ID, TICK_ID};
use crate::first_pass::read_bits::DemoParserError;
use crate::maps::NON_MULTITHREADABLE_PROPS;
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
fn normalize_segment_buckets(rb: &mut RangedBuckets) {
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
    // Variant template per prop id from any materialized column in the segment.
    let mut templates: AHashMap<u32, Variant> = AHashMap::default();
    for m in rb.dfs.iter() {
        for (id, col) in m.iter() {
            if let Some(data) = col.data.as_ref() {
                templates.entry(*id).or_insert_with(|| dummy_for(data));
            }
        }
    }
    for pmap in rb.per_player.iter() {
        for m in pmap.values() {
            for (id, col) in m.iter() {
                if let Some(data) = col.data.as_ref() {
                    templates.entry(*id).or_insert_with(|| dummy_for(data));
                }
            }
        }
    }
    if templates.is_empty() {
        return;
    }
    let mut fix_map = |m: &mut AHashMap<u32, PropColumn>| {
        for (id, col) in m.iter_mut() {
            if col.data.is_none() {
                if let Some(d) = templates.get(id) {
                    col.data = Some(empty_like(d, col.num_nones));
                    col.num_nones = 0;
                }
            }
        }
    };
    for m in rb.dfs.iter_mut() {
        fix_map(m);
    }
    for pmap in rb.per_player.iter_mut() {
        for m in pmap.values_mut() {
            fix_map(m);
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

/// Names from `wanted_player_props` that carry cross-fullpacket state and
/// therefore cannot be decoded per-segment (see `NON_MULTITHREADABLE_PROPS`).
fn non_multithreadable_requested(props: &[String]) -> Vec<String> {
    props
        .iter()
        .filter(|name| NON_MULTITHREADABLE_PROPS.contains(*name))
        .cloned()
        .collect()
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
    pub fn iter_chunks<'b>(&'b self, demo_bytes: &'b [u8], shared: SharedFirstPass) -> ChunkIter<'a, 'b> {
        self.iter_chunks_from(demo_bytes, shared, 0)
    }
    /// Same as `iter_chunks` but resumes at segment `start_idx` (an index
    /// into `shared.fullpacket_offsets`) instead of 0. The WASM `ChunkParser`
    /// handle persists its cursor across `nextChunk` calls and resumes here,
    /// so each segment is decoded exactly once. An out-of-range `start_idx`
    /// simply yields an empty iterator.
    pub fn iter_chunks_from<'b>(&'b self, demo_bytes: &'b [u8], shared: SharedFirstPass, start_idx: usize) -> ChunkIter<'a, 'b> {
        ChunkIter {
            shared,
            inputs: &self.input,
            bytes: demo_bytes,
            next_idx: start_idx,
        }
    }
    /// Parse only the segments overlapping tick range `[lo, hi]` and trim
    /// rows/events outside it. Segment decode is byte-identical to the full
    /// parse by construction (same per-segment path); out-of-range rows from
    /// the boundary segments are dropped after merging.
    pub fn parse_tick_range(
        &self,
        demo_bytes: &[u8],
        shared: SharedFirstPass,
        lo: i32,
        hi: i32,
    ) -> Result<DemoOutput, DemoParserError> {
        let seg_range = segments_for_ticks(&shared, lo, hi);
        let selected: Vec<usize> = shared.fullpacket_offsets[seg_range.clone()].to_vec();
        // Props that accumulate cross-fullpacket state (damage counters,
        // duck/crouch state, ...) reset at every per-segment boundary and
        // would diverge from the full parse. Decode the selected interval
        // CONTINUOUSLY instead: one parser from the first selected
        // fullpacket, bounded to the end of the last selected segment (the
        // next fullpacket offset, or EOF). Bound construction mirrors
        // `second_pass_threaded_with_channels`; the first fullpacket seeds
        // entity state from the snapshot and later ones apply as deltas (see
        // `parse_full_packet_and_break_if_needed`), exactly like the
        // continuous full parse.
        let mut outputs: Vec<SecondPassOutput> = if !check_multithreadability(&self.input.wanted_player_props) {
            if selected.is_empty() {
                vec![]
            } else {
                // Start one fullpacket BEFORE the first selected segment (or
                // at byte 16 when it is included): frame ticks can plateau
                // across a segment boundary, so in-range rows may physically
                // sit just before the first selected fullpacket — notably
                // around demo start, where tick-1 rows precede the first
                // tick-1 fullpacket. Frame ticks are monotonic, so one
                // segment back always covers them; the extra decode is
                // trimmed away below.
                let start = if seg_range.start <= 1 {
                    HEADER_ENDS_AT_BYTE
                } else {
                    shared.fullpacket_offsets[seg_range.start - 1]
                };
                let end = shared
                    .fullpacket_offsets
                    .get(seg_range.end)
                    .copied()
                    .unwrap_or(demo_bytes.len());
                let bound = StartEndOffset {
                    start,
                    end,
                    msg_type: StartEndType::OK,
                };
                let mut parser = SecondPassParser::new(&shared, &self.input, start, false, Some(bound))?;
                parser.start(demo_bytes)?;
                vec![parser.create_output()]
            }
        } else {
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
    /// Parse many tick ranges in ONE pass: every overlapping segment is
    /// decoded exactly once, rows/events are routed to range buckets, and
    /// event post-processing (dedup, purchase/sell) runs ONCE globally.
    /// Fixes the N× decode + N× post-proc cost of calling `parse_tick_range`
    /// per range. Rows in overlapping ranges go to the first range (sorted by
    /// `lo`); rows outside all ranges are dropped. Returned in input order.
    ///
    /// Rejects non-multithreadable props (damage counters, duck state, ...):
    /// each segment is decoded exactly once with rows routed per range, so
    /// cross-segment state cannot be preserved. Use `parse_tick_range` or
    /// the full `parse_demo` for those props.
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
    /// Like `parse_tick_ranges`, rejects non-multithreadable props.
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
        // Non-multithreadable props (damage counters, duck state, ...) carry
        // state across fullpacket boundaries, but this path decodes each
        // segment exactly once with rows routed per range, so that state
        // cannot be preserved. Fail loudly instead of returning
        // silently-wrong rows; callers needing those props should use
        // `parse_tick_range` (continuous fallback) or the full `parse_demo`.
        if !check_multithreadability(&self.input.wanted_player_props) {
            return Err(DemoParserError::NonMultithreadableProps(format!(
                "parse_tick_ranges cannot serve non-multithreadable props {:?}: per-segment decode cannot carry their state across segments; use parse_tick_range or parse_demo instead",
                non_multithreadable_requested(&self.input.wanted_player_props)
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
            seg_set.extend(segments_for_ticks(shared, lo, hi));
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

        // Assemble one DemoOutput per range (input order).
        let mut results: Vec<DemoOutput> = Vec::with_capacity(n);
        for orig in 0..n {
            let mut prop_controller = shared.prop_controller.clone();
            for prop in &shared.added_temp_props {
                prop_controller.wanted_player_props.retain(|x| x != prop);
                prop_controller.prop_infos.retain(|x| &x.prop_name != prop);
            }
            let df = self.combine_dfs(std::mem::take(&mut df_buckets[orig]), false);
            let mut pp = AHashMap::default();
            for (steamid, v) in std::mem::take(&mut pp_buckets[orig]) {
                pp.insert(steamid, self.combine_dfs(v, true));
            }
            let m = &misc[orig];
            let mut roster_by_sid: std::collections::BTreeMap<u64, PlayerEndMetaData> =
                std::collections::BTreeMap::new();
            for p in &m.roster {
                if let Some(sid) = p.steamid {
                    if sid != 0 {
                        roster_by_sid.insert(sid, p.clone());
                    }
                }
            }
            let mut all_prop_names: Vec<String> = m.uniq_prop_names.iter().cloned().collect();
            all_prop_names.sort();
            all_prop_names.dedup();
            results.push(DemoOutput {
                prop_controller,
                chat_messages: m.chat_messages.clone(),
                item_drops: m.item_drops.clone(),
                player_md: m.player_md.clone(),
                roster: roster_by_sid.into_values().collect(),
                game_events: std::mem::take(&mut events_per_range[orig]),
                skins: m.skins.clone(),
                convars: m.convars.clone(),
                df,
                header: Some(shared.header.clone()),
                game_events_counter: m.game_events_counter.clone(),
                projectiles: m.projectiles.clone(),
                voice_data: m.voice_data.clone(),
                df_per_player: pp,
                uniq_prop_names: all_prop_names,
            });
        }
        Ok(results)
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
