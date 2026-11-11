//! `deckopt status`, `deckopt apply` and `deckopt check`: the same output as the Python version.

use std::path::Path;

use serde_json::Value;

use crate::config::{load_config, user};
use crate::detect::{battery_is_low, controller_batteries, detect, panel_info, State};
use crate::input::InputWatcher;
use crate::names::GameNames;
use crate::profile::{finalize, resolve, Final};
use crate::service::{state_file, version};
use crate::tuner::Tuner;
use crate::util::now_epoch;

#[allow(non_snake_case)]
pub fn INPUT_LABELS(kind: &str) -> &'static str {
    match kind {
        "controller" => "controller",
        "kbm" => "keyboard+mouse",
        _ => "Deck controls",
    }
}

pub fn input_label(state: &State) -> String {
    let base = INPUT_LABELS(&state.input);
    if state.input != "deck" {
        format!("{base} ({})", state.input_name)
    } else {
        base.to_string()
    }
}

pub fn power_label(state: &State) -> String {
    if state.ac {
        return "charger".into();
    }
    let battery = state.battery.map(|b| format!("{b}%")).unwrap_or_else(|| "?".into());
    format!("battery {battery}{}", if state.low_battery { " LOW" } else { "" })
}

pub fn game_label(state: &State) -> Option<String> {
    state.appid.as_ref().map(|id| format!("{} ({id})", state.game.as_deref().unwrap_or("unknown game")))
}

pub fn summarize(state: &State, final_: &Final, trail: &[String]) -> String {
    let game = game_label(state).unwrap_or_else(|| "none".into());
    let power = power_label(state);
    let mut knobs = format!("tdp {}W, gpu {}, fps {}", final_.tdp, final_.gpu_mhz, if final_.fps != 0 { final_.fps.to_string() } else { "unlimited".into() });
    if let Some(hz) = final_.refresh {
        knobs += &format!(", {hz}Hz");
    }
    knobs += &format!(", smt {}, boost {}", onoff(final_.smt), onoff(final_.cpu_boost));
    if let Some(epp) = &final_.cpu_epp {
        knobs += &format!(", cpu {}", epp.replace('_', " "));
    }
    format!(
        "game: {game} | power: {power} | screen: {} | input: {}\n  -> {knobs}\n  layers: {}",
        state.screen.describe(),
        input_label(state),
        trail.join(" > ")
    )
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

pub fn default_config() -> String {
    let who = std::env::var("SUDO_USER").ok().filter(|s| !s.is_empty()).or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "deck".into());
    format!("/home/{who}/.config/deckopt/profiles.toml")
}

pub fn cmd_status(config: &Path, apply: bool, dry_run: bool) -> i32 {
    let (cfg, _) = match load_config(config) {
        Ok(c) => c,
        Err(e) => {
            crate::error!("config has an error: {e}");
            return 1;
        }
    };
    let mut names = GameNames::new(&user(&cfg));
    let mut inputs = InputWatcher::new();
    let state = detect(&cfg, &mut names, false, &mut inputs);
    let (settings, trail) = resolve(&cfg, &state);
    let final_ = finalize(&settings, &state, &panel_info());
    println!("{}", summarize(&state, &final_, &trail));
    if apply {
        Tuner::new(&cfg, dry_run).apply(&final_);
    }
    0
}

fn ago(ts: f64) -> String {
    let mins = ((now_epoch() - ts) / 60.0) as i64;
    if mins < 1 {
        "just now".into()
    } else if mins < 60 {
        format!("{mins} min ago")
    } else {
        format!("{} h {} min ago", mins / 60, mins % 60)
    }
}

fn is_tty() -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(1) == 1 }
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Plain-English health check: short enough to fit on one screen for a photo.
pub fn cmd_check(config: &Path) -> i32 {
    let cfg = match load_config(config) {
        Ok((c, _)) => c,
        Err(e) => {
            crate::error!("config has an error: {e}");
            return 1;
        }
    };
    let tty = is_tty();
    let color = |code: u8, t: &str| if tty { format!("\x1b[{code}m{t}\x1b[0m") } else { t.to_string() };
    let mark = |ok: Option<bool>| match ok {
        Some(true) => color(32, "OK "),
        Some(false) => color(31, "BAD"),
        None => color(90, " - "),
    };
    let line = |ok: Option<bool>, text: &str| println!(" {}  {text}", mark(ok));

    let data: Option<Value> = std::fs::read_to_string(state_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(|v: &Value| v.is_object());
    let shown_version = data.as_ref().and_then(|d| d.get("version")).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(version);
    println!("{}", color(1, &format!("DeckOpt check  (version {shown_version})")));
    println!();

    let active = std::process::Command::new("systemctl")
        .args(["is-active", "deckopt"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    if active == "active" {
        line(Some(true), "Background service is running");
    } else {
        line(Some(false), &format!("Background service is {}. Fix: deckopt restart", if active.is_empty() { "not installed" } else { &active }));
    }

    let mut names = GameNames::new(&user(&cfg));
    let mut inputs = InputWatcher::new();
    let state = detect(&cfg, &mut names, false, &mut inputs);
    line(None, &format!("Right now: {}, {}, {}, {}", game_label(&state).unwrap_or_else(|| "no game running".into()), power_label(&state), state.screen.describe(), INPUT_LABELS(&state.input)));

    let Some(data) = data.filter(|d| d.get("current").map(|c| !c.is_null()).unwrap_or(false)) else {
        line(Some(false), "The service hasn't recorded anything yet. Fix: deckopt restart, wait 20 seconds, check again");
        return 0;
    };
    let rec = data.get("last_game").filter(|v| !v.is_null()).or_else(|| data.get("current")).cloned().unwrap_or(Value::Null);
    println!();
    let game_mode = rec.get("game_mode").and_then(|v| v.as_bool()).unwrap_or(false);
    let where_ = if game_mode { "in Game Mode" } else { "in Desktop Mode" };
    let game = rec.get("game").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    let title = match &game {
        Some(g) => format!("While playing {g}"),
        None => "Last update".into(),
    };
    let time = rec.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0);
    println!("{}", color(1, &format!("{title} ({}, {where_}):", ago(time))));
    println!("      {}, {}, {}", str_of(rec.get("power")), str_of(rec.get("screen")), str_of(rec.get("input")));
    let labels = [
        ("tdp", "Power limit"),
        ("gpu", "GPU clock"),
        ("fps", "FPS cap"),
        ("refresh", "Refresh rate"),
        ("smt", "Hyperthreading"),
        ("cpu_boost", "CPU boost"),
        ("cpu_epp", "CPU power preference"),
        ("vrr", "VRR (adaptive sync)"),
        ("allow_tearing", "Tearing"),
        ("graphics", "In-game graphics"),
    ];
    let note_of = |v: &Value| -> Option<(Option<bool>, String)> {
        let obj = v.as_object()?;
        let ok = match obj.get("ok") {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        };
        Some((ok, str_of(obj.get("detail"))))
    };
    for (key, label) in labels {
        if let Some((ok, detail)) = rec.get("results").and_then(|r| r.get(key)).and_then(note_of) {
            line(ok, &format!("{label}: {detail}"));
        }
    }
    if let Some(problems) = rec.get("problems").and_then(|p| p.as_array()) {
        for p in problems {
            line(Some(false), &str_of(Some(p)));
        }
    }
    for (key, label) in [("memory", "Memory tweaks"), ("polling", "Controller polling")] {
        if let Some((ok, detail)) = data.get(key).and_then(note_of) {
            line(ok, &format!("{label}: {detail}"));
        }
    }
    for b in controller_batteries() {
        // live, not from the record
        let amount = b.percent.map(|p| format!("{p}%")).unwrap_or_else(|| if b.level.is_empty() { "unknown".into() } else { b.level.to_lowercase() });
        let extra = if b.charging { " (charging)" } else if battery_is_low(&b) { " - low, charge it soon" } else { "" };
        line(Some(!battery_is_low(&b)), &format!("Controller battery: {} {amount}{extra}", b.name));
    }
    if game.is_some() && !game_mode {
        line(Some(false), "Game Mode wasn't detected, so the FPS cap and refresh rate were skipped");
    }
    if data.get("last_game").map(|v| v.is_null()).unwrap_or(true) {
        line(None, "No game has been played since the service started");
    }
    if let Some(games) = data.get("graphics").and_then(|g| g.as_object()).filter(|g| !g.is_empty()) {
        println!();
        println!("{}", color(1, "Game settings (written into the game's own config, for its next launch):"));
        for g in games.values() {
            let ok = g.get("ok").and_then(|v| v.as_bool());
            let game = g.get("game").and_then(|v| v.as_str()).unwrap_or("");
            let detail = g.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            let time = g.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0);
            line(ok, &format!("{game}: {detail} ({})", ago(time)));
        }
    }
    if let Some(events) = data.get("events").and_then(|e| e.as_array()).filter(|e| !e.is_empty()) {
        println!();
        println!("{}", color(1, "Recent changes:"));
        let start = events.len().saturating_sub(6);
        for e in &events[start..] {
            println!("      {}", str_of(Some(e)));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse, BUNDLED};
    use crate::detect::OLED;
    use crate::profile::tests::{internal, state, tv};

    #[test]
    fn summary_matches_readme_format() {
        let cfg = parse(BUNDLED).unwrap();
        let s = state(Some("1245620"), Some("ELDEN RING"), true, false, tv(60.0), "deck");
        let (settings, trail) = resolve(&cfg, &s);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!(
            summarize(&s, &f, &trail),
            "game: ELDEN RING (1245620) | power: charger | screen: LG TV SSCR2 (TV, DP-1 1920x1080@60Hz) | input: Deck controls\n  -> tdp 15W, gpu auto, fps 30, smt on, boost on, cpu balance performance\n  layers: defaults > profile > profile.ac > profile.external > game.\"1245620\".external"
        );
        let s = state(None, None, false, true, internal(), "kbm");
        let (settings, trail) = resolve(&cfg, &s);
        let f = finalize(&settings, &s, &OLED);
        assert_eq!(
            summarize(&s, &f, &trail),
            "game: none | power: battery 10% LOW | screen: Built-in (eDP-1 1280x800@90Hz) | input: keyboard+mouse (Logitech K400 Plus)\n  -> tdp 7W, gpu auto, fps 30, 90Hz, smt on, boost off, cpu power\n  layers: defaults > profile > profile.battery > profile.low_battery"
        );
    }
}
