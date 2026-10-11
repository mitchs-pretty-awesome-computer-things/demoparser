use parser::first_pass::parser::SharedFirstPass;
use parser::first_pass::parser_settings::rm_user_friendly_names;
use parser::first_pass::parser_settings::FirstPassParser;
use parser::first_pass::parser_settings::ParserInputs;
use parser::parse_demo::Parser;
use parser::parse_demo::ParsingMode::ForceSingleThreaded;
use parser::second_pass::parser_settings::create_huffman_lookup_table;
use parser::second_pass::variants::soa_to_aos;
use parser::second_pass::variants::OutputSerdeHelperStruct;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::result::Result;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn parseEvent(
    file: Vec<u8>,
    event_name: Option<String>,
    wanted_player_props: Option<Vec<JsValue>>,
    wanted_other_props: Option<Vec<JsValue>>,
) -> Result<JsValue, JsError> {
    let player_props = match wanted_player_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let other_props = match wanted_other_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let real_names_player = match rm_user_friendly_names(&player_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let real_other_props = match rm_user_friendly_names(&other_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };

    let mut real_name_to_og_name = HashMap::default();
    for (real_name, user_friendly_name) in real_names_player.iter().zip(&player_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    for (real_name, user_friendly_name) in real_other_props.iter().zip(&other_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let settings = ParserInputs {
        wanted_players: vec![],
        wanted_player_props: real_names_player,
        wanted_other_props: real_other_props,
        real_name_to_og_name: real_name_to_og_name.into(),
        wanted_events: vec![event_name.unwrap_or("none".to_string())],
        collect_ticks: false,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: vec![],
        parse_projectiles: false,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf,
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&file) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    match serde_wasm_bindgen::to_value(&output.game_events) {
        Ok(s) => Ok(s),
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    }
}
#[wasm_bindgen]
pub fn parseEvents(
    file: Vec<u8>,
    event_names: Option<Vec<JsValue>>,
    wanted_player_props: Option<Vec<JsValue>>,
    wanted_other_props: Option<Vec<JsValue>>,
) -> Result<JsValue, JsError> {
    let event_names = match event_names {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let player_props = match wanted_player_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let other_props = match wanted_other_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let real_names_player = match rm_user_friendly_names(&player_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let real_other_props = match rm_user_friendly_names(&other_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };

    let mut real_name_to_og_name = HashMap::default();
    for (real_name, user_friendly_name) in real_names_player.iter().zip(&player_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    for (real_name, user_friendly_name) in real_other_props.iter().zip(&other_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let settings = ParserInputs {
        wanted_players: vec![],
        wanted_player_props: real_names_player,
        wanted_other_props: real_other_props,
        real_name_to_og_name: real_name_to_og_name.into(),
        wanted_events: event_names,
        collect_ticks: false,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: vec![],
        parse_projectiles: false,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf,
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&file) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    match serde_wasm_bindgen::to_value(&output.game_events) {
        Ok(s) => Ok(s),
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    }
}

#[wasm_bindgen]
pub fn listGameEvents(fileBytes: Vec<u8>) -> Result<JsValue, JsError> {
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let settings = ParserInputs {
        wanted_players: vec![],
        real_name_to_og_name: HashMap::default().into(),
        wanted_player_props: vec![],
        wanted_other_props: vec![],
        wanted_events: vec!["all".to_string()],
        collect_ticks: false,
        parse_voice: false,
        parse_ents: false,
        wanted_ticks: vec![],
        parse_projectiles: false,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&fileBytes) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let v = Vec::from_iter(output.game_events_counter.iter());
    match serde_wasm_bindgen::to_value(&v) {
        Ok(s) => Ok(s),
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    }
}
#[wasm_bindgen]
pub fn listUpdatedFields(fileBytes: Vec<u8>) -> Result<JsValue, JsError> {
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let settings = ParserInputs {
        wanted_players: vec![],
        real_name_to_og_name: HashMap::default().into(),
        wanted_player_props: vec![],
        wanted_other_props: vec![],
        wanted_events: vec!["none".to_string()],
        collect_ticks: false,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: vec![],
        parse_projectiles: false,
        only_header: false,
        list_props: true,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&fileBytes) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let v = Vec::from_iter(output.uniq_prop_names.iter());
    match serde_wasm_bindgen::to_value(&v) {
        Ok(s) => Ok(s),
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    }
}

#[wasm_bindgen]
pub fn parseTicks(
    file: Vec<u8>,
    wanted_props: Option<Vec<JsValue>>,
    wanted_ticks: Option<Vec<i32>>,
    wanted_players: Option<Vec<JsValue>>,
    struct_of_arrays: Option<bool>,
) -> Result<JsValue, JsError> {
    let wanted_props = match wanted_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let wanted_players_u64 = match wanted_players {
        Some(v) => v
            .iter()
            .map(|x| x.as_string().unwrap().parse::<u64>().unwrap_or(0))
            .collect(),
        None => vec![],
    };
    let mut real_names = match rm_user_friendly_names(&wanted_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let mut real_name_to_og_name = HashMap::default();
    for (real_name, user_friendly_name) in real_names.iter().zip(&wanted_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    let wanted_ticks = match wanted_ticks {
        Some(t) => t,
        None => vec![],
    };
    let settings = ParserInputs {
        wanted_players: wanted_players_u64,
        real_name_to_og_name: real_name_to_og_name.into(),
        wanted_player_props: real_names.clone(),
        wanted_other_props: vec![],
        wanted_events: vec![],
        collect_ticks: true,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: wanted_ticks,
        parse_projectiles: false,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&file) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let mut prop_infos = output.prop_controller.prop_infos.clone();
    prop_infos.sort_by_key(|x| x.prop_name.clone());

    let helper = OutputSerdeHelperStruct {
        prop_infos: prop_infos,
        inner: output.df.into(),
    };

    let is_soa = match struct_of_arrays {
        Some(true) => true,
        _ => false,
    };

    if is_soa {
        let s = match serde_wasm_bindgen::to_value(&helper) {
            Ok(s) => s,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        return Ok(s);
    } else {
        let result = soa_to_aos(helper, false);
        let s = match serde_wasm_bindgen::to_value(&result) {
            Ok(s) => s,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        Ok(s)
    }
}

/// Bounded-memory slice of `parseTicks` for browsers: only the fullpacket
/// segments overlapping `[tick_lo, tick_hi]` are decoded and only rows in
/// range are returned, so peak memory stays proportional to the slice —
/// never the whole demo. Same row shape as `parseTicks`.
#[wasm_bindgen]
pub fn parseTickRange(
    file: Vec<u8>,
    wanted_props: Option<Vec<JsValue>>,
    tick_lo: i32,
    tick_hi: i32,
    wanted_players: Option<Vec<JsValue>>,
    struct_of_arrays: Option<bool>,
) -> Result<JsValue, JsError> {
    let wanted_props = match wanted_props {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let wanted_players_u64 = match wanted_players {
        Some(v) => v
            .iter()
            .map(|x| x.as_string().unwrap().parse::<u64>().unwrap_or(0))
            .collect(),
        None => vec![],
    };
    let real_names = match rm_user_friendly_names(&wanted_props) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let arc_huf = Arc::new(create_huffman_lookup_table());
    let mut real_name_to_og_name = HashMap::default();
    for (real_name, user_friendly_name) in real_names.iter().zip(&wanted_props) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    let settings = ParserInputs {
        wanted_players: wanted_players_u64,
        real_name_to_og_name: real_name_to_og_name.into(),
        wanted_player_props: real_names.clone(),
        wanted_other_props: vec![],
        wanted_events: vec![],
        collect_ticks: true,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: vec![],
        parse_projectiles: false,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let parser = Parser::new(settings, ForceSingleThreaded);

    let shared = match parser.preparse(&file) {
        Ok(shared) => shared,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let output = match parser.parse_tick_range(&file, shared, tick_lo, tick_hi) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let mut prop_infos = output.prop_controller.prop_infos.clone();
    prop_infos.sort_by_key(|x| x.prop_name.clone());

    let helper = OutputSerdeHelperStruct {
        prop_infos: prop_infos,
        inner: output.df.into(),
    };

    let is_soa = match struct_of_arrays {
        Some(true) => true,
        _ => false,
    };

    if is_soa {
        let s = match serde_wasm_bindgen::to_value(&helper) {
            Ok(s) => s,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        return Ok(s);
    } else {
        let result = soa_to_aos(helper, false);
        let s = match serde_wasm_bindgen::to_value(&result) {
            Ok(s) => s,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        Ok(s)
    }
}
/// extra: lets you add new fields to grenades. Use list_updated_fields for a full list.
/// grenades: lets you disable non-projectile grenades. This can have a big difference on memory/speed.
/// grenade_classes: when grenades is enabled, only emit rows for these non-projectile
/// grenade classes (e.g. inferno flames). Omit to emit every non-projectile class.
/// skip_nones: when true, omit missing props from rows instead of emitting explicit nulls.
/// Grenade rows are heterogeneous (projectiles have no fire props, burns have no
/// coordinates), so with wide extras like the 64 CInferno fire nodes explicit nulls
/// multiply every row. Defaults to false to preserve legacy null behavior.
#[wasm_bindgen]
pub fn parseGrenades(
    file: Vec<u8>,
    extra: Option<Vec<JsValue>>,
    grenades: Option<bool>,
    grenade_classes: Option<Vec<JsValue>>,
    skip_nones: Option<bool>,
) -> Result<JsValue, JsError> {
    let mut extra = match extra {
        Some(p) => p.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>(),
        None => vec![],
    };
    let mut real_names = match rm_user_friendly_names(&extra) {
        Ok(names) => names,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let grenades = grenades.unwrap_or(true);
    let grenade_classes = grenade_classes
        .map(|classes| classes.iter().map(|s| s.as_string().unwrap()).collect::<Vec<_>>());
    let skip_nones = skip_nones.unwrap_or(false);

    let arc_huf = Arc::new(create_huffman_lookup_table());
    let mut real_name_to_og_name = HashMap::default();
    for (real_name, user_friendly_name) in real_names.iter().zip(&extra) {
        real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
    }
    let settings = ParserInputs {
        wanted_players: vec![],
        real_name_to_og_name: real_name_to_og_name.into(),
        wanted_player_props: vec![],
        wanted_other_props: real_names.clone(),
        wanted_events: vec![],
        collect_ticks: true,
        parse_voice: false,
        parse_ents: true,
        wanted_ticks: vec![],
        parse_projectiles: true,
        only_header: false,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: grenades,
        grenade_classes: grenade_classes,
    };
    let mut parser = Parser::new(settings, ForceSingleThreaded);

    let output = match parser.parse_demo(&file) {
        Ok(output) => output,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    let mut prop_infos = output.prop_controller.prop_infos.clone();
    prop_infos.sort_by_key(|x| x.prop_name.clone());
    let helper = OutputSerdeHelperStruct {
        prop_infos: prop_infos,
        inner: output.df.into(),
    };
    // Grenade rows are heterogeneous (projectiles have no fire props, burns
    // have no coordinates). When skip_nones is set, missing props are omitted
    // rather than emitted as nulls: with wide extras like the 64 CInferno fire
    // nodes, explicit nulls would multiply every row by the extra count.
    // Defaults to false to preserve legacy null behavior.
    let result = soa_to_aos(helper, skip_nones);
    let s = match serde_wasm_bindgen::to_value(&result) {
        Ok(s) => s,
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    };
    Ok(s)
}

#[wasm_bindgen]
pub fn parseHeader(file: Vec<u8>) -> Result<JsValue, JsError> {
    let arc_huf = Arc::new(create_huffman_lookup_table());

    let settings = ParserInputs {
        wanted_players: vec![],
        real_name_to_og_name: HashMap::default().into(),
        wanted_player_props: vec![],
        wanted_other_props: vec![],
        wanted_events: vec![],
        collect_ticks: true,
        parse_voice: false,
        parse_ents: false,
        wanted_ticks: vec![],
        parse_projectiles: true,
        only_header: true,
        list_props: false,
        only_convars: false,
        huffman_lookup_table: &arc_huf.clone(),
        order_by_steamid: false,
        wanted_prop_states: HashMap::default().into(),
        fallback_bytes: None,
        parse_grenades: false,
        grenade_classes: None,
    };
    let mut parser = FirstPassParser::new(&settings);
    let output = parser.parse_header_only(&file).unwrap();
    let mut hm: HashMap<String, String> = HashMap::default();
    hm.extend(output);
    match serde_wasm_bindgen::to_value(&hm) {
        Ok(s) => Ok(s),
        Err(e) => return Err(JsError::new(&format!("{}", e))),
    }
}

/// Options for `ChunkParser` (the `opts` constructor argument), deserialized
/// from JS via `serde_wasm_bindgen::from_value`.
#[derive(serde::Deserialize)]
struct ChunkOpts {
    /// Ticks mode: player prop friendly names (e.g. X, Y, Z). Grenades mode:
    /// extra prop names (e.g. "m_fireCount", "m_firePositions").
    #[serde(rename = "wantedProps")]
    wanted_props: Vec<String>,
    /// Grenades mode only: non-projectile classes to emit (e.g. ["CInferno"]).
    #[serde(rename = "grenadeClasses")]
    grenade_classes: Option<Vec<String>>,
    /// Grenades mode only: emit non-projectile grenades. Default true,
    /// mirroring the `grenades` flag of `parseGrenades`.
    #[serde(rename = "nonProjectileGrenades")]
    non_projectile_grenades: Option<bool>,
    /// Grenades mode only: omit missing props instead of emitting nulls.
    /// Default false, mirroring `parseGrenades`.
    #[serde(rename = "skipNones")]
    skip_nones: Option<bool>,
}

/// Bounded-memory streaming handle over one demo: the file is copied into
/// wasm memory ONCE, preparsed ONCE, and each `nextChunk` call decodes exactly
/// one fullpacket segment and returns its rows — so peak memory stays
/// proportional to one segment instead of the whole demo. Row shape matches
/// `parseTicks` (ticks mode) or `parseGrenades` (grenades mode).
///
/// One handle serves exactly one mode; the consumer creates one handle per
/// pass, so one `Parser` + one preparse per handle is sufficient.
#[wasm_bindgen]
pub struct ChunkParser {
    inner: Option<ChunkParserInner>,
}

/// Owned state behind a `ChunkParser` handle. Kept in an `Option` so `free`
/// can release the demo bytes, parser, and preparse snapshot promptly while
/// leaving the JS wrapper alive to report a clear error on further use.
struct ChunkParserInner {
    file: Vec<u8>,
    parser: Parser<'static>,
    shared: SharedFirstPass,
    next_idx: usize,
    chunk_count: usize,
    skip_nones: bool,
}

#[wasm_bindgen]
impl ChunkParser {
    /// `file`: whole demo bytes (copied once, shared by all chunks).
    /// `mode`: "ticks" or "grenades". `wanted_ticks`: ticks mode only, the
    /// exact wanted tick list (`None`/empty means all ticks); ignored for
    /// grenades mode. `opts`: `{ wantedProps, grenadeClasses?,
    /// nonProjectileGrenades?, skipNones? }`.
    #[wasm_bindgen(constructor)]
    pub fn new(file: Vec<u8>, mode: String, wanted_ticks: Option<Vec<i32>>, opts: JsValue) -> Result<ChunkParser, JsError> {
        let opts: ChunkOpts = match serde_wasm_bindgen::from_value(opts) {
            Ok(opts) => opts,
            Err(e) => return Err(JsError::new(&format!("invalid opts: {}", e))),
        };
        let is_grenades = match mode.as_str() {
            "ticks" => false,
            "grenades" => true,
            other => return Err(JsError::new(&format!("invalid mode {:?}: expected \"ticks\" or \"grenades\"", other))),
        };
        let real_names = match rm_user_friendly_names(&opts.wanted_props) {
            Ok(names) => names,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        let mut real_name_to_og_name = HashMap::default();
        for (real_name, user_friendly_name) in real_names.iter().zip(&opts.wanted_props) {
            real_name_to_og_name.insert(real_name.clone(), user_friendly_name.clone());
        }
        // The huffman table must outlive the stored `Parser`. The one-shot
        // bindings borrow a stack-local table because `Parser::new` doesn't
        // retain it, but a struct-stored parser needs a `'static` borrow: leak
        // one small table per handle. The handle is short-lived and
        // `free`/`Drop` releases everything else, so the leaked table is the
        // only per-handle cost that outlives it.
        let huf: &'static Vec<(u8, u8)> = Box::leak(Box::new(create_huffman_lookup_table()));
        // Settings mirror `parseTicks` (ticks mode) and `parseGrenades`
        // (grenades mode) exactly so chunk rows match the one-shot shape.
        let settings = if is_grenades {
            ParserInputs {
                wanted_players: vec![],
                real_name_to_og_name: real_name_to_og_name.into(),
                wanted_player_props: vec![],
                wanted_other_props: real_names.clone(),
                wanted_events: vec![],
                collect_ticks: true,
                parse_voice: false,
                parse_ents: true,
                wanted_ticks: vec![],
                parse_projectiles: true,
                only_header: false,
                list_props: false,
                only_convars: false,
                huffman_lookup_table: huf,
                order_by_steamid: false,
                wanted_prop_states: HashMap::default().into(),
                fallback_bytes: None,
                parse_grenades: opts.non_projectile_grenades.unwrap_or(true),
                grenade_classes: opts.grenade_classes,
            }
        } else {
            ParserInputs {
                wanted_players: vec![],
                real_name_to_og_name: real_name_to_og_name.into(),
                wanted_player_props: real_names.clone(),
                wanted_other_props: vec![],
                wanted_events: vec![],
                collect_ticks: true,
                parse_voice: false,
                parse_ents: true,
                wanted_ticks: wanted_ticks.unwrap_or_default(),
                parse_projectiles: false,
                only_header: false,
                list_props: false,
                only_convars: false,
                huffman_lookup_table: huf,
                order_by_steamid: false,
                wanted_prop_states: HashMap::default().into(),
                fallback_bytes: None,
                parse_grenades: false,
                grenade_classes: None,
            }
        };
        let parser = Parser::new(settings, ForceSingleThreaded);
        let shared = match parser.preparse(&file) {
            Ok(shared) => shared,
            Err(e) => return Err(JsError::new(&format!("{}", e))),
        };
        let chunk_count = shared.fullpacket_offsets.len();
        Ok(ChunkParser {
            inner: Some(ChunkParserInner {
                file,
                parser,
                shared,
                next_idx: 0,
                chunk_count,
                skip_nones: if is_grenades { opts.skip_nones.unwrap_or(false) } else { false },
            }),
        })
    }

    /// Number of segments (fullpacket offsets) in this demo.
    #[wasm_bindgen(getter, js_name = chunkCount)]
    pub fn chunk_count(&self) -> usize {
        self.inner.as_ref().map(|inner| inner.chunk_count).unwrap_or(0)
    }

    /// Decode the next fullpacket segment and return its rows (`undefined`
    /// once exhausted). The per-tick gate in `collect_entities` already
    /// filters rows to `wanted_ticks`, so no post-trim is needed. Errors
    /// after `free` instead of touching released state.
    ///
    /// Returns `Result<JsValue, JsError>` rather than
    /// `Result<Option<JsValue>, JsError>`: this wasm-bindgen version cannot
    /// return `Option` inside `Result`. Exhaustion is still `undefined` on
    /// the JS side, matching what `Option` would generate.
    #[wasm_bindgen(js_name = nextChunk)]
    pub fn next_chunk(&mut self) -> Result<JsValue, JsError> {
        let inner = match self.inner.as_mut() {
            Some(inner) => inner,
            None => return Err(JsError::new("ChunkParser has been freed")),
        };
        if inner.next_idx >= inner.chunk_count {
            return Ok(JsValue::UNDEFINED);
        }
        let idx = inner.next_idx;
        inner.next_idx += 1;
        // `ChunkIter` owns its snapshot, so hand it a clone. The per-segment
        // `SecondPassParser::new` clones the same tables again per segment, so
        // this matches the existing per-chunk clone cost (no asymptotic change).
        let mut iter = inner.parser.iter_chunks_from(&inner.file, inner.shared.clone(), idx);
        let segment = match iter.next() {
            Some(Ok(output)) => output,
            Some(Err(e)) => return Err(JsError::new(&format!("{}", e))),
            // Unreachable: `idx` was bounds-checked above, but report cleanly.
            None => return Ok(JsValue::UNDEFINED),
        };
        // Same serialization as `parseTickRange`: shared prop infos sorted by
        // prop name, then struct-of-arrays to array-of-objects. The segment
        // `df` is the same `AHashMap<u32, PropColumn>` type as `DemoOutput`'s.
        let mut prop_infos = inner.shared.prop_controller.prop_infos.clone();
        prop_infos.sort_by_key(|x| x.prop_name.clone());
        let helper = OutputSerdeHelperStruct {
            prop_infos: prop_infos,
            inner: segment.df.into(),
        };
        let result = soa_to_aos(helper, inner.skip_nones);
        serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&format!("{}", e)))
    }

    /// Release the demo bytes, parser, and preparse snapshot promptly.
    /// Idempotent; `nextChunk` after `free` reports a clear error.
    ///
    /// Note: wasm-bindgen also generates its own `free()` (which destroys the
    /// JS wrapper); ours is emitted last in the class body, so it wins at
    /// runtime and the wrapper stays alive for the post-free error path. The
    /// struct shell itself is reclaimed by the FinalizationRegistry hook.
    pub fn free(&mut self) {
        self.inner = None;
    }
}

impl Drop for ChunkParser {
    /// Safety net so a handle dropped without `free` still releases its
    /// (potentially large) demo bytes and snapshot promptly.
    fn drop(&mut self) {
        self.inner = None;
    }
}
