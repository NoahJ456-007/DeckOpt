//! The background service: `deckopt run`. Watches the game, the charger and the screen, re-tunes
//! whenever any of them changes, re-sends the frame cap, and records what it did for `deckopt check`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::adaptive::{fps_ladder, AdaptiveFPS};
use crate::autotdp::AutoTDP;
use crate::cli::{game_label, input_label, power_label, summarize, INPUT_LABELS};
use crate::config::{load_config, setting_bool, setting_f64, setting_i64, user, Config};
use crate::detect::{battery_is_low, controller_batteries, detect, panel_info, ControllerBattery, Panel, State};
use crate::graphics;
use crate::input::InputWatcher;
use crate::learned::Learned;
use crate::names::GameNames;
use crate::profile::{best_refresh, finalize, resolve, Final};
use crate::tuner::{Note, Tuner};
use crate::util::{boottime, local_hhmm, monotonic, now_epoch, read};
use crate::{debug, error, info, warning};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub time: f64,
    pub game: Option<String>,
    pub power: String,
    pub screen: String,
    pub input: String,
    pub target: Final,
    pub results: IndexMap<String, Note>,
    pub problems: Vec<String>,
    pub game_mode: bool,
}

/// What the service did, for `deckopt check` to read without root (state.json).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub started: f64,
    pub current: Option<Record>,
    pub last_game: Option<Record>,
    pub events: Vec<String>,
    pub memory: Option<Note>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub polling: Option<Note>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub controller_batteries: Option<Vec<ControllerBattery>>,
    #[serde(default)]
    pub graphics: indexmap::IndexMap<String, graphics::GraphicsResult>,
}

pub fn app_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

pub fn state_file() -> PathBuf {
    match std::env::var("DECKOPT_STATE") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => app_dir().join("state.json"),
    }
}

pub fn learned_file() -> PathBuf {
    state_file().with_file_name("learned.json")
}

pub fn version() -> String {
    read(app_dir().join("VERSION")).filter(|v| !v.is_empty()).unwrap_or_else(|| "dev".into())
}

pub fn save_state(status: &Status) {
    let path = state_file();
    let tmp = path.with_extension("tmp");
    let result = (|| -> std::io::Result<()> {
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        serde::Serialize::serialize(status, &mut ser).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, buf)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = result {
        debug!("could not save state: {e}");
    }
}

fn record(status: &mut Status, state: &State, final_: &Final, tuner: &Tuner, problems: Vec<String>) {
    let rec = Record {
        time: now_epoch(),
        game: game_label(state),
        power: power_label(state),
        screen: state.screen.describe(),
        input: input_label(state),
        target: final_.clone(),
        results: tuner.results.clone(),
        problems,
        game_mode: tuner.results.get("fps").map(|r| r.ok.is_some()).unwrap_or(false),
    };
    if state.appid.is_some() {
        status.last_game = Some(rec.clone());
    }
    status.current = Some(rec);
    save_state(status);
}

fn set_cap(final_: &mut Final, state: &State, panel: &Panel, adaptive: &AdaptiveFPS) {
    final_.fps = adaptive.cap;
    if state.screen.internal {
        let (lo, hi) = panel.refresh;
        final_.refresh = Some(best_refresh(adaptive.cap, lo, hi, true));
    }
}

fn note_adaptive(tuner: &mut Tuner, final_: &Final, adaptive: &AdaptiveFPS) {
    if adaptive.key.is_none() || !tuner.result_ok("fps") {
        return;
    }
    let detail = if let Some(e) = &adaptive.perf_error {
        format!("{} (adaptive is off: {e})", final_.fps)
    } else {
        let measured = adaptive.measured.map(|m| format!(", game running at {m:.0}")).unwrap_or_default();
        format!("{} (adaptive, aiming for {}{measured})", final_.fps, adaptive.ladder.first().copied().unwrap_or(final_.fps))
    };
    if let Some(r) = tuner.results.get_mut("fps") {
        r.detail = detail;
    }
}

fn note_auto(tuner: &mut Tuner, final_: &Final) {
    if let Some(max) = final_.tdp_max {
        if tuner.result_ok("tdp") {
            if let Some(r) = tuner.results.get_mut("tdp") {
                r.detail = format!("{}W (auto-tuned, up to {max}W)", final_.tdp);
            }
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: the handler only touches an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

fn sleep_interruptible(seconds: f64) {
    let end = monotonic() + seconds;
    while !STOP.load(Ordering::SeqCst) {
        let left = end - monotonic();
        if left <= 0.0 {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(left.min(0.2)));
    }
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn run(config: &Path, dry_run: bool) -> i32 {
    let (cfg, path) = match load_config(config) {
        Ok(c) => c,
        Err(e) => {
            error!("config has an error: {e}");
            return 1;
        }
    };
    install_signal_handlers();
    let mut tuner = Tuner::new(&cfg, dry_run);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_loop(cfg, path, &mut tuner, dry_run)));
    // everything changed is put back by tuner.restore(), even after a crash. The games' own settings files
    // are not: those only go back on `deckopt toggle` (off) and uninstall (see graphics::restore_all).
    tuner.restore();
    match result {
        Ok(code) => code,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn run_loop(mut cfg: Config, path: PathBuf, tuner: &mut Tuner, dry_run: bool) -> i32 {
    let mut cfg_mtime = mtime(&path);
    let mut names = GameNames::new(&user(&cfg));
    let mut learned = Learned::load(learned_file());
    let mut auto = AutoTDP::new(&cfg);
    let mut adaptive = AdaptiveFPS::new();
    let mut inputs = InputWatcher::new();
    let panel = panel_info();
    info!("DeckOpt started ({} panel, config {})", panel.model, path.display());
    let mut status = Status { version: version(), started: now_epoch(), current: None, last_game: None, events: Vec::new(), memory: None, polling: None, controller_batteries: None, graphics: Default::default() };
    // CLOCK_MONOTONIC stops while asleep and CLOCK_BOOTTIME doesn't, so a jump in the gap means we just woke up.
    let mut sleep_gap = boottime().map(|b| b - monotonic()).unwrap_or(0.0);

    let mut last: Option<(Option<String>, bool, bool, crate::detect::Screen, String)> = None;
    let mut low = false;
    let mut applied_at = 0.0;
    let mut settled = true;
    let mut verified_at = 0.0;
    let mut final_ = Final::stock();
    let mut gfx = graphics::Graphics::new(&cfg, dry_run);
    let mut prev_appid: Option<String> = None;
    let mut graphics_key: Option<(bool, bool, crate::detect::Screen, String)> = None;
    let mut graphics_due: Option<f64> = Some(0.0); // due straight away at start (if no game runs)
    if setting_bool(&cfg, "memory_tweaks", false) {
        tuner.apply_memory_tweaks();
        status.memory = tuner.memory_result.clone();
    }
    if setting_bool(&cfg, "fast_controller_polling", false) {
        status.polling = Some(tuner.fast_controller_polling());
    }
    let mut warned_batteries: Vec<String> = Vec::new();
    while !STOP.load(Ordering::SeqCst) {
        if path.exists() && mtime(&path) != cfg_mtime {
            cfg_mtime = mtime(&path);
            match load_config(&path) {
                Ok((new_cfg, _)) => {
                    cfg = new_cfg;
                    last = None; // force a re-apply with the new profiles
                    graphics_key = None;
                    info!("config reloaded");
                }
                Err(e) => error!("config has an error, keeping the old one: {e}"),
            }
        }
        let poll_seconds = setting_f64(&cfg, "poll_seconds", 3.0);
        let settle_seconds = setting_f64(&cfg, "settle_seconds", 8.0);
        let verify_seconds = setting_f64(&cfg, "verify_seconds", 15.0);

        if let Some(b) = boottime() {
            let gap = b - monotonic();
            if gap - sleep_gap > 5.0 {
                info!("woke from sleep, re-applying settings");
                last = None; // some settings reset during suspend
                auto.prev_cpu = None;
            }
            sleep_gap = gap;
        }

        let state = detect(&cfg, &mut names, low, &mut inputs);
        if setting_bool(&cfg, "keep_controllers_awake", true) {
            tuner.keep_controllers_awake(&inputs.devices);
        }
        let batteries = controller_batteries();
        for b in &batteries {
            if battery_is_low(b) && !warned_batteries.contains(&b.name) {
                warned_batteries.push(b.name.clone());
                let amount = b.percent.map(|p| format!("{p}%")).unwrap_or_else(|| b.level.clone());
                warning!("controller battery low: {} {}", b.name, amount);
            } else if !battery_is_low(b) {
                warned_batteries.retain(|n| n != &b.name);
            }
        }
        if status.controller_batteries.as_ref() != Some(&batteries) {
            status.controller_batteries = Some(batteries);
            if status.current.is_some() {
                save_state(&status);
            }
        }
        low = state.low_battery;
        let key = state.key();
        let now = monotonic();
        tuner.set_charge_limit(state.ac && !state.screen.internal, setting_i64(&cfg, "docked_charge_limit", 0));
        // The games' own settings files: written only while no game runs, for the next launch. A game that has
        // just quit has rewritten its file, so ours go in again a moment later.
        let gkey = (key.1, key.2, key.3.clone(), key.4.clone());
        if state.appid.is_none() && prev_appid.is_some() {
            graphics_due = Some(now + 3.0);
        } else if state.appid.is_none() && graphics_due.is_none() && graphics_key.as_ref() != Some(&gkey) {
            graphics_due = Some(now);
        }
        prev_appid = state.appid.clone();
        if state.appid.is_none() && graphics_due.is_some_and(|due| now >= due) {
            gfx.apply_all(&cfg, &state, &mut |id| names.get(id));
            status.graphics = gfx.results.clone();
            save_state(&status);
            graphics_key = Some(gkey);
            graphics_due = None;
        }
        // Steam applies its own per-game settings a few seconds after launch, so apply once more after that.
        let resettle = !settled && now - applied_at >= settle_seconds;
        if last.as_ref() != Some(&key) || resettle {
            let (values, trail) = resolve(&cfg, &state);
            final_ = finalize(&values, &state, &panel);
            let changed = last.as_ref() != Some(&key);
            if final_.auto_tdp && state.appid.is_some() {
                final_.tdp_max = Some(final_.tdp);
                if changed || auto.key.is_none() {
                    let k = format!("{}|{}|{}", state.appid.as_deref().unwrap_or(""), state.power(), state.where_());
                    auto.begin(Some(&k), final_.tdp, &learned);
                }
                final_.tdp = auto.current;
            } else {
                auto.begin(None, 0, &learned);
            }
            let env = tuner.gamescope_env();
            if final_.adaptive_fps && state.appid.is_some() && env.is_some() && final_.fps > 0 {
                if changed || adaptive.key.is_none() {
                    let appid = state.appid.as_deref().unwrap_or("0");
                    let key_name = format!("fps|{appid}|{}|{}", state.power(), state.where_());
                    adaptive.begin(Some(&key_name), appid.parse().unwrap_or(0), fps_ladder(final_.fps, &state, &panel, 30), Some(&learned));
                }
                set_cap(&mut final_, &state, &panel, &adaptive);
            } else {
                adaptive.begin(None, 0, Vec::new(), None);
            }
            if changed {
                info!("{}", summarize(&state, &final_, &trail));
                let knobs = format!("{}W, {} FPS", final_.tdp, if final_.fps != 0 { final_.fps.to_string() } else { "unlimited".into() });
                status.events.push(format!(
                    "{}  {} | {} | {} | {} -> {knobs}",
                    local_hhmm(),
                    game_label(&state).unwrap_or_else(|| "no game".into()),
                    power_label(&state),
                    state.screen.name,
                    INPUT_LABELS(&state.input)
                ));
                let n = status.events.len();
                if n > 8 {
                    status.events.drain(..n - 8);
                }
            }
            tuner.apply(&final_);
            note_auto(tuner, &final_);
            note_adaptive(tuner, &final_, &adaptive);
            for (key_name, r) in &tuner.results {
                if r.ok == Some(false) {
                    warning!("{key_name} not applied: {}", r.detail);
                }
            }
            settled = !changed;
            applied_at = now;
            last = Some(key);
            verified_at = if resettle { 0.0 } else { now }; // check right after the settle pass
            record(&mut status, &state, &final_, tuner, Vec::new());
        }
        let env = tuner.gamescope_env();
        let new_cap = adaptive.sample(tuner, &mut learned, env.as_ref());
        if new_cap.is_some() {
            set_cap(&mut final_, &state, &panel, &adaptive);
            let env = tuner.gamescope_env().unwrap_or_default();
            let (err, _) = tuner.send_fps(&env, final_.fps);
            let detail = err.clone().unwrap_or_else(|| final_.fps.to_string());
            tuner.note("fps", Some(err.is_none()), detail);
            if let Some(hz) = final_.refresh {
                tuner.note("refresh", Some(true), format!("{hz}Hz (Game Mode matches it to the FPS cap)"));
            }
            note_adaptive(tuner, &final_, &adaptive);
            let problems = status.current.as_ref().map(|c| c.problems.clone()).unwrap_or_default();
            record(&mut status, &state, &final_, tuner, problems);
        } else if adaptive.key.is_some() && adaptive.measured.is_some() && tuner.result_ok("fps") {
            note_adaptive(tuner, &final_, &adaptive); // keep the measured frame rate in the check fresh
        }
        if let Some(new_tdp) = auto.sample(tuner, &mut learned) {
            info!("auto power limit for {}: {}W -> {new_tdp}W", game_label(&state).unwrap_or_else(|| "None".into()), final_.tdp);
            final_.tdp = new_tdp;
            tuner.set_tdp(new_tdp);
            note_auto(tuner, &final_);
            let problems = status.current.as_ref().map(|c| c.problems.clone()).unwrap_or_default();
            record(&mut status, &state, &final_, tuner, problems);
        }
        // Watch for Steam (or anything else) changing our settings afterwards.
        if settled && now - verified_at >= verify_seconds {
            verified_at = now;
            let problems = if dry_run { Vec::new() } else { tuner.verify(&final_) };
            let previous = status.current.as_ref().map(|c| c.problems.clone()).unwrap_or_default();
            if problems != previous {
                for p in &problems {
                    warning!("{p}");
                }
            }
            record(&mut status, &state, &final_, tuner, problems);
        }
        sleep_interruptible(poll_seconds);
    }
    0
}
