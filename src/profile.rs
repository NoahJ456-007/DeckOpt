//! Profile resolution: layer the profile tables for the current state, then turn symbolic values
//! ("refresh", "max", "auto") into concrete numbers for this screen.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::config::{as_bool, as_int, Config};
use crate::detect::{Panel, State};
use crate::util::clamp;
use crate::warning;

pub const TDP_RANGE: (i64, i64) = (3, 15);
pub const GPU_MHZ_RANGE: (i64, i64) = (200, 1600);
pub const SETTING_KEYS: &[&str] = &["tdp", "auto_tdp", "gpu_mhz", "fps", "adaptive_fps", "max_fps", "refresh", "smt", "cpu_boost", "cpu_epp", "vrr", "allow_tearing"];

/// Resolved-but-symbolic settings (Python's `DEFAULTS` dict updated by the layers). Keys whose default
/// is None (cpu_epp, vrr, allow_tearing) are simply absent.
pub type Settings = IndexMap<String, Value>;

pub fn defaults() -> Settings {
    let mut d = Settings::new();
    d.insert("tdp".into(), Value::Integer(15));
    d.insert("auto_tdp".into(), Value::Boolean(false));
    d.insert("gpu_mhz".into(), Value::String("auto".into()));
    d.insert("fps".into(), Value::Integer(0));
    d.insert("adaptive_fps".into(), Value::Boolean(false));
    d.insert("max_fps".into(), Value::Integer(60));
    d.insert("refresh".into(), Value::String("max".into()));
    d.insert("smt".into(), Value::Boolean(true));
    d.insert("cpu_boost".into(), Value::Boolean(true));
    d
}

/// Finds a table by any of the keys, case-insensitively. Returns the key as written in the file.
pub fn find_table<'a>(tables: Option<&'a Value>, keys: &[Option<&str>]) -> Option<(&'a str, &'a Table)> {
    let tables = tables?.as_table()?;
    for key in keys.iter().flatten() {
        let want = key.to_lowercase();
        for (k, v) in tables {
            if k.to_lowercase() == want {
                return v.as_table().map(|t| (k.as_str(), t));
            }
        }
    }
    None
}

/// [controller."..."] tables match the controller's name (any part of it, any case) or its vendor id,
/// alone ("3537") or with the product ("3537:1022").
pub fn find_controller<'a>(tables: Option<&'a Value>, state: &State) -> Option<(&'a str, &'a Table)> {
    let tables = tables?.as_table()?;
    let name = state.input_name.to_lowercase();
    let vendor = state.input_id.split(':').next().unwrap_or("");
    for (key, table) in tables {
        let k = key.to_lowercase();
        if name.contains(&k) || k == state.input_id || k == vendor {
            return table.as_table().map(|t| (key.as_str(), t));
        }
    }
    None
}

/// Merge one profile table and its condition sub-tables, most general first.
fn merge_layer(table: Option<&Table>, state: &State, label: &str, out: &mut Settings, trail: &mut Vec<String>) {
    let Some(table) = table else { return };
    let kind: Option<&str> = if state.screen.internal { None } else { Some(state.screen.kind.as_str()) };
    let power = state.power();
    let where_ = state.where_();
    let suffixes: Vec<Option<String>> = vec![
        Some(String::new()),
        Some(power.into()),
        Some(where_.into()),
        kind.map(String::from),
        Some(state.input.clone()),
        Some(format!("{power}+{where_}")),
        kind.map(|k| format!("{power}+{k}")),
        Some(format!("{power}+{}", state.input)),
    ];
    for suffix in suffixes.into_iter().flatten() {
        let sub = if suffix.is_empty() { Some(table) } else { table.get(&suffix).and_then(|v| v.as_table()) };
        let Some(sub) = sub else { continue };
        let values: Vec<(&String, &Value)> = sub.iter().filter(|(k, _)| SETTING_KEYS.contains(&k.as_str())).collect();
        if !values.is_empty() {
            for (k, v) in values {
                out.insert(k.clone(), v.clone());
            }
            trail.push(if suffix.is_empty() { label.to_string() } else { format!("{label}.{suffix}") });
        }
    }
}

pub fn resolve(cfg: &Config, state: &State) -> (Settings, Vec<String>) {
    let mut out = defaults();
    let mut trail = vec!["defaults".to_string()];
    let profile = cfg.get("profile").and_then(|v| v.as_table());
    merge_layer(profile, state, "profile", &mut out, &mut trail);
    if let Some((key, table)) = find_table(cfg.get("display"), &[Some(&state.screen.name), Some(&state.screen.connector)]) {
        merge_layer(Some(table), state, &format!("display.\"{key}\""), &mut out, &mut trail);
    }
    if state.input == "controller" {
        if let Some((key, table)) = find_controller(cfg.get("controller"), state) {
            merge_layer(Some(table), state, &format!("controller.\"{key}\""), &mut out, &mut trail);
        }
    }
    let game = find_table(cfg.get("game"), &[state.appid.as_deref(), state.game.as_deref()]);
    if let Some((key, table)) = game {
        merge_layer(Some(table), state, &format!("game.\"{key}\""), &mut out, &mut trail);
    }
    if state.low_battery {
        // Low battery is a safety cap, so it wins over the game profile unless the game sets its own.
        let mut layers: Vec<(String, Option<&Table>)> = vec![("profile.low_battery".into(), profile.and_then(|p| p.get("low_battery")).and_then(|v| v.as_table()))];
        if let Some((key, table)) = game {
            layers.push((format!("game.\"{key}\".low_battery"), table.get("low_battery").and_then(|v| v.as_table())));
        }
        for (label, table) in layers {
            if let Some(table) = table {
                for (k, v) in table.iter().filter(|(k, _)| SETTING_KEYS.contains(&k.as_str())) {
                    out.insert(k.clone(), v.clone());
                }
                trail.push(label);
            }
        }
    }
    (out, trail)
}

/// Refresh rate in range that's a whole multiple of the fps cap, for even frame pacing.
/// Lowest such rate by default (least power), highest with highest=true (least latency).
pub fn best_refresh(fps: i64, lo: i64, hi: i64, highest: bool) -> i64 {
    if fps <= 0 {
        return hi;
    }
    let rates: Vec<i64> = if highest { (lo..=hi).rev().collect() } else { (lo..=hi).collect() };
    rates.into_iter().find(|hz| hz % fps == 0).unwrap_or(hi)
}

/// Highest frame cap at or under `limit` that the refresh rate divides evenly, so every frame is shown
/// for the same number of refreshes (144Hz -> 72 or 48, 120Hz -> 60, 60Hz -> 60 or 30).
/// Falls back to `limit` if the best even cap would be below limit * min_ratio (e.g. 75Hz -> 25).
pub fn even_fps(hz: f64, limit: i64, min_ratio: f64) -> i64 {
    for n in 1..13 {
        let fps = hz / n as f64;
        let rounded = fps.round_ties_even();
        if fps <= limit as f64 + 0.5 && (fps - rounded).abs() < 0.1 {
            return if fps >= limit as f64 * min_ratio { rounded as i64 } else { limit };
        }
    }
    limit
}

#[derive(Clone, Debug, PartialEq)]
pub enum Fps {
    Auto,
    Refresh,
    Half,
    Num(i64),
}

fn fps_value(v: Option<&Value>) -> Fps {
    match v {
        Some(Value::String(s)) if s == "auto" => Fps::Auto,
        Some(Value::String(s)) if s == "refresh" => Fps::Refresh,
        Some(Value::String(s)) if s == "half" => Fps::Half,
        Some(v) => match as_int(v) {
            Some(n) => Fps::Num(n),
            None => {
                warning!("fps = {} is not a number, treating it as 0", v);
                Fps::Num(0)
            }
        },
        None => Fps::Num(0),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GpuClock {
    Auto(String), // always "auto"
    Mhz(i64),
}

impl std::fmt::Display for GpuClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuClock::Auto(_) => write!(f, "auto"),
            GpuClock::Mhz(m) => write!(f, "{m}"),
        }
    }
}

/// Concrete settings for this screen (Python's `finalize` dict). Serialized into state.json as "target".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Final {
    pub tdp: i64,
    pub auto_tdp: bool,
    pub adaptive_fps: bool,
    pub gpu_mhz: GpuClock,
    pub fps: i64,
    pub refresh: Option<i64>, // gamescope drives external refresh itself
    pub smt: bool,
    pub cpu_boost: bool,
    pub cpu_epp: Option<String>,
    pub vrr: Option<bool>, // None = leave Steam's setting alone
    pub allow_tearing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tdp_max: Option<i64>,
}

impl Final {
    /// What `restore()` applies: `{**DEFAULTS, "fps": 0, "refresh": None}`.
    pub fn stock() -> Final {
        Final { tdp: 15, auto_tdp: false, adaptive_fps: false, gpu_mhz: GpuClock::Auto("auto".into()), fps: 0, refresh: None, smt: true, cpu_boost: true, cpu_epp: None, vrr: None, allow_tearing: None, tdp_max: None }
    }
}

fn opt_bool(v: Option<&Value>) -> Option<bool> {
    v.map(as_bool)
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

/// Turn symbolic values ("refresh", "max", "auto", ...) into concrete numbers for this screen.
pub fn finalize(settings: &Settings, state: &State, panel: &Panel) -> Final {
    let (lo, hi) = panel.refresh;
    let max_fps = settings.get("max_fps").and_then(as_int).unwrap_or(60);
    let mut fps = fps_value(settings.get("fps"));
    let hz: i64;
    if state.screen.internal {
        if fps == Fps::Auto {
            fps = Fps::Num(max_fps.min(hi));
        }
        // Game Mode sets the refresh rate itself: the highest rate the frame cap divides evenly, so
        // 40fps -> 80Hz and 60fps -> 60Hz on the OLED (no judder). The `refresh` setting is no longer used.
        hz = match fps {
            Fps::Refresh | Fps::Half => hi,
            Fps::Num(n) => best_refresh(n, lo, hi, true),
            Fps::Auto => unreachable!(),
        };
    } else {
        // External screens keep their own refresh rate, so fit the frame cap to it instead.
        let live = if state.screen.refresh != 0.0 { state.screen.refresh } else { 60.0 };
        match fps {
            Fps::Auto => fps = Fps::Num(even_fps(live, max_fps, 0.75)),
            Fps::Num(n) if n > 0 => fps = Fps::Num(even_fps(live, n, 0.85)),
            _ => {}
        }
        hz = live.round_ties_even() as i64;
    }
    let fps_num = match fps {
        Fps::Refresh => hz,
        Fps::Half => hz / 2,
        Fps::Num(n) => n,
        Fps::Auto => 0,
    };
    let gpu = match settings.get("gpu_mhz") {
        Some(Value::String(s)) if s == "auto" => GpuClock::Auto("auto".into()),
        Some(v) => GpuClock::Mhz(clamp(as_int(v).unwrap_or(GPU_MHZ_RANGE.0), GPU_MHZ_RANGE.0, GPU_MHZ_RANGE.1)),
        None => GpuClock::Auto("auto".into()),
    };
    let cpu_epp = opt_str(settings.get("cpu_epp")).filter(|s| !s.is_empty());
    Final {
        tdp: clamp(settings.get("tdp").and_then(as_int).unwrap_or(15), TDP_RANGE.0, TDP_RANGE.1),
        auto_tdp: settings.get("auto_tdp").map(as_bool).unwrap_or(false),
        adaptive_fps: settings.get("adaptive_fps").map(as_bool).unwrap_or(false),
        gpu_mhz: gpu,
        fps: fps_num.max(0),
        refresh: if state.screen.internal { Some(hz) } else { None },
        smt: settings.get("smt").map(as_bool).unwrap_or(true),
        cpu_boost: settings.get("cpu_boost").map(as_bool).unwrap_or(true),
        cpu_epp,
        vrr: opt_bool(settings.get("vrr")),
        allow_tearing: opt_bool(settings.get("allow_tearing")),
        tdp_max: None,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::{parse, BUNDLED};
    use crate::detect::{Screen, LCD, OLED};

    pub fn state(appid: Option<&str>, game: Option<&str>, ac: bool, low: bool, screen: Screen, input: &str) -> State {
        State {
            appid: appid.map(String::from),
            game: game.map(String::from),
            ac,
            battery: Some(if low { 10 } else { 80 }),
            low_battery: low,
            screen,
            input: input.into(),
            input_name: match input { "controller" => "GameSir-Nova Lite".into(), "kbm" => "Logitech K400 Plus".into(), _ => "".into() },
            input_id: match input { "controller" => "3537:1022".into(), "kbm" => "046d:c52b".into(), _ => "".into() },
        }
    }

    pub fn internal() -> Screen {
        Screen { connector: "eDP-1".into(), internal: true, name: "Built-in".into(), width: 1280, height: 800, refresh: 90.0, kind: "built-in".into() }
    }

    pub fn tv(hz: f64) -> Screen {
        Screen { connector: "DP-1".into(), internal: false, name: "LG TV SSCR2".into(), width: 1920, height: 1080, refresh: hz, kind: "tv".into() }
    }

    pub fn monitor(hz: f64) -> Screen {
        Screen { connector: "DP-1".into(), internal: false, name: "DELL S2721DGF".into(), width: 2560, height: 1440, refresh: hz, kind: "monitor".into() }
    }

    #[test]
    fn refresh_and_even_fps() {
        assert_eq!(best_refresh(40, 45, 90, true), 80);
        assert_eq!(best_refresh(60, 45, 90, true), 60);
        assert_eq!(best_refresh(40, 40, 60, true), 40);
        assert_eq!(best_refresh(45, 45, 90, true), 90);
        assert_eq!(best_refresh(0, 45, 90, true), 90);
        assert_eq!(best_refresh(7, 40, 60, false), 42);
        assert_eq!(even_fps(60.0, 60, 0.75), 60);
        assert_eq!(even_fps(120.0, 60, 0.75), 60);
        assert_eq!(even_fps(144.0, 60, 0.75), 48);
        assert_eq!(even_fps(165.0, 60, 0.75), 55);
        assert_eq!(even_fps(144.0, 40, 0.85), 36);
        assert_eq!(even_fps(75.0, 30, 0.85), 30); // 25 would be below 30*0.85
        assert_eq!(even_fps(59.94, 60, 0.75), 60);
        assert_eq!(even_fps(60.0, 30, 0.85), 30);
    }

    #[test]
    fn bundled_layers_match_python() {
        let cfg = parse(BUNDLED).unwrap();
        // Hades on battery, built-in OLED, Deck controls.
        let s = state(Some("1145360"), Some("Hades"), false, false, internal(), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        assert_eq!(trail, vec!["defaults", "profile", "profile.battery", "game.\"1145360\"", "game.\"1145360\".battery"]);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!((f.tdp, f.fps, f.refresh, f.auto_tdp, f.adaptive_fps), (6, 60, Some(60), true, false));
        assert_eq!(f.cpu_epp.as_deref(), Some("balance_power"));
        assert_eq!(f.smt, true);

        // Unknown game on the charger, built-in: adaptive 60.
        let s = state(Some("999"), None, true, false, internal(), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        assert_eq!(trail, vec!["defaults", "profile", "profile.ac"]);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!((f.tdp, f.fps, f.refresh, f.adaptive_fps), (15, 60, Some(60), true));
        let f = finalize(&settings, &s, &LCD);
        assert_eq!((f.fps, f.refresh), (60, Some(60)));

        // Elden Ring on a 60Hz TV with a controller on the charger.
        let s = state(Some("1245620"), Some("ELDEN RING"), true, false, tv(60.0), "controller");
        let (settings, trail) = resolve(&cfg, &s);
        // [game.1245620] itself has no settings (only sub-tables), so it's not a layer.
        assert_eq!(trail, vec!["defaults", "profile", "profile.ac", "profile.external", "game.\"1245620\".external"]);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!((f.tdp, f.fps, f.refresh, f.vrr), (15, 30, None, Some(true)));

        // Keyboard and mouse on a 144Hz monitor on the charger: fps = "refresh" -> 144.
        let s = state(Some("999"), None, true, false, monitor(144.0), "kbm");
        let (settings, trail) = resolve(&cfg, &s);
        assert_eq!(trail, vec!["defaults", "profile", "profile.ac", "profile.external", "profile.ac+kbm"]);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!(f.fps, 144);

        // Same monitor with a controller: fps = "auto" -> 48.
        let s = state(Some("999"), None, true, false, monitor(144.0), "controller");
        let (settings, _) = resolve(&cfg, &s);
        assert_eq!(finalize(&settings, &s, &OLED).fps, 48);

        // Low battery: safety cap wins over the game profile.
        let s = state(Some("1145360"), Some("Hades"), false, true, internal(), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        assert_eq!(trail.last().unwrap(), "profile.low_battery");
        let f = finalize(&settings, &s, &LCD);
        assert_eq!((f.tdp, f.fps, f.refresh, f.cpu_boost), (7, 30, Some(60), false));
        assert_eq!(f.cpu_epp.as_deref(), Some("power"));

        // Terraria: fixed 60 with refresh = "fps" -> 60Hz on the OLED.
        let s = state(Some("105600"), Some("Terraria"), true, false, internal(), "deck");
        let (settings, _) = resolve(&cfg, &s);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!((f.tdp, f.fps, f.refresh), (9, 60, Some(60)));
    }

    #[test]
    fn user_overrides_and_name_matching() {
        let user = parse("[game.\"hades\"]\ntdp = 12\n[display.\"lg tv sscr2\"]\nfps = 30\n[controller.\"3537\"]\nfps = 20\n[profile.battery]\ntdp = 8\n").unwrap();
        let cfg = crate::config::deep_merge(&parse(BUNDLED).unwrap(), &user);
        let s = state(Some("1145360"), Some("Hades"), false, false, internal(), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        // appid key wins over the name key because it's checked first
        assert!(trail.contains(&"game.\"1145360\"".to_string()));
        assert_eq!(finalize(&settings, &s, &OLED).tdp, 6);
        let s = state(Some("424242"), Some("Hades"), false, false, internal(), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        assert!(trail.contains(&"game.\"hades\"".to_string()));
        assert_eq!(finalize(&settings, &s, &OLED).tdp, 12);
        let s = state(None, None, true, false, tv(60.0), "controller");
        let (settings, trail) = resolve(&cfg, &s);
        assert_eq!(trail, vec!["defaults", "profile", "profile.ac", "profile.external", "display.\"lg tv sscr2\"", "controller.\"3537\""]);
        assert_eq!(finalize(&settings, &s, &OLED).fps, 20);
    }

    #[test]
    fn stock_final() {
        let f = Final::stock();
        assert_eq!(serde_json::to_string(&f).unwrap(), "{\"tdp\":15,\"auto_tdp\":false,\"adaptive_fps\":false,\"gpu_mhz\":\"auto\",\"fps\":0,\"refresh\":null,\"smt\":true,\"cpu_boost\":true,\"cpu_epp\":null,\"vrr\":null,\"allow_tearing\":null}");
    }
}
