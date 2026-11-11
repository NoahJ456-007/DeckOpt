//! Adaptive FPS. While plugged in: aim for the profile's cap (60 by default). If the game can't hold it,
//! step down to the next evenly paced cap; once it sits comfortably at a lower cap with GPU to spare,
//! try one step up, and back off for a while if that doesn't hold. Uses real frame times from gamescope.
//! The level each game settles at is remembered per power source and screen.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::detect::{Panel, State};
use crate::gamescope::GamescopePerf;
use crate::learned::Learned;
use crate::tuner::{Env, Tuner};
use crate::util::{is_digits, monotonic, read_or, sysfs};
use crate::{debug, info, warning};

pub const WARMUP: f64 = 30.0;
pub const WINDOW: f64 = 10.0;
pub const UP_AFTER: f64 = 60.0;
pub const COOLDOWN: f64 = 300.0;

/// Evenly paced caps from `top` down: each has a refresh rate it divides exactly.
/// OLED: 60 -> 45 -> 40 -> 30; LCD: 60 -> 45 -> 40 -> 30; 60Hz TV: 60 -> 30; 120Hz: 60 -> 40 -> 30.
pub fn fps_ladder(top: i64, state: &State, panel: &Panel, floor: i64) -> Vec<i64> {
    let mut steps: Vec<i64> = if state.screen.internal {
        let (lo, hi) = panel.refresh;
        let mut steps = vec![top];
        steps.extend([60, 45, 40, 30].into_iter().filter(|&f| f < top));
        steps.into_iter().filter(|&f| f > 0 && (lo..=hi).any(|hz| hz % f == 0)).collect()
    } else {
        let hz = if state.screen.refresh != 0.0 { state.screen.refresh } else { 60.0 };
        let mut steps = vec![top];
        for n in 1..13 {
            let f = hz / n as f64;
            let r = f.round_ties_even();
            if (f - r).abs() < 0.1 && floor - 5 <= r as i64 && (r as i64) < top {
                steps.push(r as i64);
            }
        }
        steps
    };
    steps.sort_unstable_by(|a, b| b.cmp(a));
    steps.dedup();
    if steps.is_empty() {
        steps.push(top); // Python would crash on ladder[0]; keep the cap as the only level instead
    }
    steps
}

/// The pure decision for one window: (new cap, reason) or None. `now`, `last_change` in seconds.
pub struct Window<'a> {
    pub ladder: &'a [i64],
    pub cap: i64,
    pub measured: f64,
    pub gpu: &'a [i64],
    pub now: f64,
    pub last_change: f64,
    pub low_windows: &'a mut i64,
    pub trial_from: &'a mut Option<i64>,
    pub cooldown_until: &'a mut HashMap<i64, f64>,
    pub failures: &'a mut HashMap<i64, i64>,
}

pub fn decide(w: Window) -> Option<(i64, String)> {
    let i = w.ladder.iter().position(|&c| c == w.cap)?;
    if w.measured < w.cap as f64 * 0.9 {
        *w.low_windows += 1;
        if *w.low_windows >= 2 && i + 1 < w.ladder.len() {
            if w.trial_from.is_some() && w.now - w.last_change < 2.0 * UP_AFTER {
                // The step up didn't hold. Wait before trying again, twice as long after each failure,
                // so a game that really can't hold it isn't put through a rough patch every few minutes.
                let fails = w.failures.entry(w.cap).or_insert(0);
                *fails += 1;
                w.cooldown_until.insert(w.cap, w.now + COOLDOWN * 2f64.powi((*fails - 1) as i32));
            }
            *w.trial_from = None;
            return Some((w.ladder[i + 1], format!("averaging {:.0}", w.measured)));
        }
        return None;
    }
    *w.low_windows = 0;
    let headroom = !w.gpu.is_empty() && (w.gpu.iter().sum::<i64>() as f64 / w.gpu.len() as f64) < 80.0;
    if i > 0 && w.now - w.last_change >= UP_AFTER && w.measured >= w.cap as f64 * 0.97 && headroom && w.cooldown_until.get(&w.ladder[i - 1]).copied().unwrap_or(0.0) < w.now {
        *w.trial_from = Some(w.cap);
        return Some((w.ladder[i - 1], "steady with GPU to spare, trying higher".into()));
    }
    None
}

pub struct AdaptiveFPS {
    pub key: Option<String>,
    pub appid: u64,
    pub ladder: Vec<i64>,
    pub cap: i64,
    pub measured: Option<f64>,
    pub perf_error: Option<String>,
    perf: Option<GamescopePerf>,
    perf_retry: f64,
    readings: Vec<(f64, f64)>,
    gpu: Vec<(f64, i64)>,
    started: f64,
    last_change: f64,
    last_decision: f64,
    low_windows: i64,
    trial_from: Option<i64>,
    cooldown_until: HashMap<i64, f64>,
    failures: HashMap<i64, i64>,
}

impl Default for AdaptiveFPS {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveFPS {
    pub fn new() -> AdaptiveFPS {
        let mut a = AdaptiveFPS {
            key: None,
            appid: 0,
            ladder: Vec::new(),
            cap: 0,
            measured: None,
            perf_error: None,
            perf: None,
            perf_retry: 0.0,
            readings: Vec::new(),
            gpu: Vec::new(),
            started: 0.0,
            last_change: 0.0,
            last_decision: 0.0,
            low_windows: 0,
            trial_from: None,
            cooldown_until: HashMap::new(),
            failures: HashMap::new(),
        };
        a.begin(None, 0, Vec::new(), None);
        a
    }

    pub fn begin(&mut self, key: Option<&str>, appid: u64, ladder: Vec<i64>, learned: Option<&Learned>) {
        self.key = key.map(String::from);
        self.appid = appid;
        if let Some(k) = key {
            let remembered = learned.and_then(|l| l.get(k));
            self.cap = match remembered {
                Some(r) if ladder.contains(&r) => r,
                _ => ladder.first().copied().unwrap_or(0),
            };
        }
        self.ladder = ladder;
        let now = monotonic();
        self.readings.clear();
        self.gpu.clear();
        self.started = now;
        self.last_change = now;
        self.last_decision = now;
        self.low_windows = 0;
        self.trial_from = None;
        self.cooldown_until.clear();
        self.failures.clear();
        self.measured = None;
    }

    fn connect(&mut self, env: &Env) -> bool {
        if self.perf.is_some() || monotonic() < self.perf_retry {
            return self.perf.is_some();
        }
        let runtime = env.get("XDG_RUNTIME_DIR").cloned().unwrap_or_else(|| "/run/user/1000".into());
        let display = env.get("GAMESCOPE_WAYLAND_DISPLAY").cloned().unwrap_or_else(|| "gamescope-0".into());
        let path = sysfs(&PathBuf::from(runtime).join(display).to_string_lossy());
        match GamescopePerf::connect(&path) {
            Ok(p) => {
                self.perf = Some(p);
                self.perf_error = None;
                true
            }
            Err(e) => {
                warning!("adaptive FPS can't read frame times: {e}");
                self.perf = None;
                self.perf_error = Some(e);
                self.perf_retry = monotonic() + 60.0;
                false
            }
        }
    }

    /// Call every poll. Returns a new cap when it should change, else None.
    pub fn sample(&mut self, tuner: &mut Tuner, learned: &mut Learned, env: Option<&Env>) -> Option<i64> {
        let key = self.key.clone()?;
        let env = env?;
        if !self.connect(env) {
            return None;
        }
        let fps = match self.perf.as_mut().unwrap().fps(self.appid, 0.5) {
            Ok(f) => f,
            Err(e) => {
                debug!("frame time read failed: {e}");
                self.perf = None;
                return None;
            }
        };
        let now = monotonic();
        if let Some(f) = fps {
            self.readings.push((now, f));
        }
        if let Some(h) = tuner.amdgpu_hwmon() {
            let busy = read_or(tuner.gpu_device(&h).join("gpu_busy_percent"), "");
            if is_digits(&busy) {
                if let Ok(b) = busy.parse::<i64>() {
                    self.gpu.push((now, b));
                }
            }
        }
        if now - self.started < WARMUP || now - self.last_decision < WINDOW {
            return None;
        }
        self.last_decision = now;
        self.readings.retain(|(t, _)| now - t <= WINDOW);
        self.gpu.retain(|(t, _)| now - t <= WINDOW);
        let mut recent: Vec<f64> = self.readings.iter().map(|(_, f)| *f).collect();
        recent.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let gpu: Vec<i64> = self.gpu.iter().map(|(_, g)| *g).collect();
        if recent.len() < 3 {
            return None; // paused, loading, or not drawing
        }
        let measured = recent[recent.len() / 2];
        self.measured = Some(measured);
        let decision = decide(Window {
            ladder: &self.ladder,
            cap: self.cap,
            measured,
            gpu: &gpu,
            now,
            last_change: self.last_change,
            low_windows: &mut self.low_windows,
            trial_from: &mut self.trial_from,
            cooldown_until: &mut self.cooldown_until,
            failures: &mut self.failures,
        });
        let (cap, why) = decision?;
        info!("adaptive FPS: {} -> {} ({})", self.cap, cap, why);
        self.cap = cap;
        self.last_change = monotonic();
        self.low_windows = 0;
        self.measured = None;
        learned.set(&key, cap);
        learned.save();
        Some(cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{LCD, OLED};
    use crate::profile::tests::{internal, state, tv};

    #[test]
    fn ladders_match_python() {
        let s = state(Some("1"), None, true, false, internal(), "deck");
        assert_eq!(fps_ladder(60, &s, &OLED, 30), vec![60, 45, 40, 30]);
        assert_eq!(fps_ladder(60, &s, &LCD, 30), vec![60, 45, 40, 30]);
        assert_eq!(fps_ladder(90, &s, &OLED, 30), vec![90, 60, 45, 40, 30]);
        assert_eq!(fps_ladder(40, &s, &LCD, 30), vec![40, 30]);
        assert_eq!(fps_ladder(100, &s, &OLED, 30), vec![60, 45, 40, 30]); // 100 has no multiple in 45..90
        let s = state(Some("1"), None, true, false, tv(60.0), "controller");
        assert_eq!(fps_ladder(60, &s, &OLED, 30), vec![60, 30]);
        let s = state(Some("1"), None, true, false, tv(120.0), "controller");
        assert_eq!(fps_ladder(60, &s, &OLED, 30), vec![60, 40, 30]);
        let s = state(Some("1"), None, true, false, tv(144.0), "controller");
        assert_eq!(fps_ladder(48, &s, &OLED, 30), vec![48, 36]); // 144/5 = 28.8 isn't even, 24 is below floor-5
        let s = state(Some("1"), None, true, false, tv(59.94), "controller");
        assert_eq!(fps_ladder(60, &s, &OLED, 30), vec![60, 30]);
    }

    fn run(ladder: &[i64], cap: i64, measured: f64, gpu: &[i64], now: f64, last_change: f64, low: &mut i64, trial: &mut Option<i64>, cd: &mut HashMap<i64, f64>, fails: &mut HashMap<i64, i64>) -> Option<(i64, String)> {
        decide(Window { ladder, cap, measured, gpu, now, last_change, low_windows: low, trial_from: trial, cooldown_until: cd, failures: fails })
    }

    #[test]
    fn step_down_needs_two_low_windows() {
        let ladder = [60, 45, 40, 30];
        let (mut low, mut trial, mut cd, mut fails) = (0, None, HashMap::new(), HashMap::new());
        assert_eq!(run(&ladder, 60, 52.0, &[90], 100.0, 0.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        assert_eq!(low, 1);
        let d = run(&ladder, 60, 50.0, &[90], 110.0, 0.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d, Some((45, "averaging 50".into())));
        // 54 is not below 60*0.9 = 54
        low = 0;
        assert_eq!(run(&ladder, 60, 54.0, &[90], 120.0, 0.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        assert_eq!(low, 0);
        // at the bottom there's nowhere to go
        low = 5;
        assert_eq!(run(&ladder, 30, 20.0, &[99], 130.0, 0.0, &mut low, &mut trial, &mut cd, &mut fails), None);
    }

    #[test]
    fn step_up_and_exponential_cooldown() {
        let ladder = [60, 45, 40, 30];
        let (mut low, mut trial, mut cd, mut fails) = (0, None, HashMap::new(), HashMap::new());
        // steady at 45, GPU busy: no step up
        assert_eq!(run(&ladder, 45, 45.0, &[85, 90], 1000.0, 900.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        // too soon after the last change
        assert_eq!(run(&ladder, 45, 45.0, &[50], 1000.0, 950.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        // 43.6 < 45 * 0.97
        assert_eq!(run(&ladder, 45, 43.6, &[50], 1000.0, 900.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        // steady with headroom: trial up
        let d = run(&ladder, 45, 45.0, &[50], 1000.0, 900.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d, Some((60, "steady with GPU to spare, trying higher".into())));
        assert_eq!(trial, Some(45));
        // the trial fails within 2*UP_AFTER: cooldown 300s on 60, doubling each failure
        low = 1;
        let d = run(&ladder, 60, 50.0, &[95], 1050.0, 1000.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d.map(|d| d.0), Some(45));
        assert_eq!(fails[&60], 1);
        assert_eq!(cd[&60], 1050.0 + 300.0);
        assert_eq!(trial, None);
        // during the cooldown no step up even when steady
        assert_eq!(run(&ladder, 45, 45.0, &[50], 1200.0, 1050.0, &mut low, &mut trial, &mut cd, &mut fails), None);
        // after it, try again; fail again -> 600s
        let d = run(&ladder, 45, 45.0, &[50], 1400.0, 1050.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d.map(|d| d.0), Some(60));
        low = 1;
        let d = run(&ladder, 60, 48.0, &[95], 1450.0, 1400.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d.map(|d| d.0), Some(45));
        assert_eq!(fails[&60], 2);
        assert_eq!(cd[&60], 1450.0 + 600.0);
        // a step down long after a trial is not a failed trial
        let (mut low, mut trial, mut cd, mut fails) = (1, Some(45), HashMap::new(), HashMap::new());
        let d = run(&ladder, 60, 48.0, &[95], 2000.0, 1500.0, &mut low, &mut trial, &mut cd, &mut fails);
        assert_eq!(d.map(|d| d.0), Some(45));
        assert!(fails.is_empty());
    }

    #[test]
    fn remembered_cap_is_used_if_on_the_ladder() {
        let mut l = Learned { path: Default::default(), data: Default::default() };
        l.set("fps|1|ac|internal", 40);
        let mut a = AdaptiveFPS::new();
        a.begin(Some("fps|1|ac|internal"), 1, vec![60, 45, 40, 30], Some(&l));
        assert_eq!(a.cap, 40);
        l.set("fps|1|ac|internal", 50);
        a.begin(Some("fps|1|ac|internal"), 1, vec![60, 45, 40, 30], Some(&l));
        assert_eq!(a.cap, 60);
    }
}
