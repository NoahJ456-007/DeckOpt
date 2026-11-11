//! Battery saver that works for every game, with or without a profile.
//!
//! Watches how busy the GPU and the busiest CPU core are. Plenty of idle time means the game is
//! easily hitting its frame cap, so the power limit steps down; the GPU or a core maxing out means it's
//! struggling, so it steps back up. It never goes above the profile's tdp, and what it settles on is
//! remembered per game, power source and screen, so the next session starts there.

use std::collections::HashMap;

use crate::config::{setting_i64, Config};
use crate::debug;
use crate::learned::Learned;
use crate::tuner::Tuner;
use crate::util::{clamp, is_digits, monotonic, read_or, sysfs};

pub const WARMUP: f64 = 20.0;
pub const STEP_EVERY: f64 = 10.0;

pub type CpuTimes = HashMap<String, (i64, i64)>; // core -> (total, idle + iowait)

pub fn parse_cpu_times(text: &str) -> CpuTimes {
    let mut times = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        if !(name.starts_with("cpu") && name.len() > 3 && name[3..].bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let f: Vec<i64> = parts.filter_map(|x| x.parse().ok()).collect();
        if f.len() < 4 {
            continue;
        }
        times.insert(name.to_string(), (f.iter().sum(), f[3] + if f.len() > 4 { f[4] } else { 0 }));
    }
    times
}

pub fn cpu_times() -> CpuTimes {
    parse_cpu_times(&read_or(sysfs("/proc/stat"), ""))
}

/// One step of the decision: the new limit given the window's averages, or None to stay put.
pub fn decide(gpu: f64, core: f64, current: i64, floor: i64, ceiling: i64) -> Option<i64> {
    let new = if gpu > 92.0 || core > 92.0 {
        ceiling.min(current + 2) // struggling: recover quickly
    } else if gpu < 70.0 && core < 80.0 {
        floor.max(current - 1) // headroom: ease down
    } else {
        current
    };
    (new != current).then_some(new)
}

pub struct AutoTDP {
    pub floor: i64,
    pub key: Option<String>,
    pub ceiling: i64,
    pub current: i64,
    samples: Vec<(i64, f64)>,
    pub prev_cpu: Option<CpuTimes>,
    started: f64,
    last_step: f64,
}

impl AutoTDP {
    pub fn new(cfg: &Config) -> AutoTDP {
        let mut a = AutoTDP { floor: setting_i64(cfg, "auto_tdp_min", 5), key: None, ceiling: 0, current: 0, samples: Vec::new(), prev_cpu: None, started: 0.0, last_step: 0.0 };
        a.begin(None, 0, &Learned { path: Default::default(), data: Default::default() });
        a
    }

    pub fn begin(&mut self, key: Option<&str>, ceiling: i64, learned: &Learned) {
        self.key = key.map(String::from);
        self.ceiling = ceiling;
        if let Some(k) = key {
            let remembered = learned.get(k).unwrap_or(ceiling);
            self.current = clamp(remembered, self.floor.min(ceiling), ceiling);
        }
        self.samples.clear();
        self.prev_cpu = None;
        self.started = monotonic();
        self.last_step = self.started;
    }

    /// Call every poll. Returns a new power limit when it's time to change it, else None.
    pub fn sample(&mut self, tuner: &mut Tuner, learned: &mut Learned) -> Option<i64> {
        let key = self.key.clone()?;
        let busy = tuner.amdgpu_hwmon().map(|h| read_or(tuner.gpu_device(&h).join("gpu_busy_percent"), "")).unwrap_or_default();
        let cpu = cpu_times();
        if let (true, Ok(busy), Some(prev)) = (is_digits(&busy), busy.parse::<i64>(), &self.prev_cpu) {
            let cores = cpu.iter().filter_map(|(c, (total, idle))| {
                let (ptotal, pidle) = prev.get(c)?;
                Some(100.0 * (1.0 - (idle - pidle) as f64 / (total - ptotal).max(1) as f64))
            });
            let busiest = cores.fold(0.0_f64, f64::max);
            self.samples.push((busy, busiest));
        }
        self.prev_cpu = Some(cpu);
        let now = monotonic();
        if now - self.started < WARMUP || now - self.last_step < STEP_EVERY || self.samples.is_empty() {
            return None;
        }
        let n = self.samples.len() as f64;
        let gpu = self.samples.iter().map(|(g, _)| *g as f64).sum::<f64>() / n;
        let core = self.samples.iter().map(|(_, c)| *c).sum::<f64>() / n;
        self.samples.clear();
        self.last_step = now;
        let decision = decide(gpu, core, self.current, self.floor, self.ceiling);
        debug!("auto tdp: gpu {}%, busiest core {}%, {}W -> {}W", gpu as i64, core as i64, self.current, decision.unwrap_or(self.current));
        let new = decision?;
        self.current = new;
        learned.set(&key, new);
        learned.save();
        Some(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing() {
        let t = parse_cpu_times("cpu  100 0 50 800 20 0 5 0 0 0\ncpu0 10 0 5 80 2 0 1 0 0 0\ncpu1 20 0 10 60 10 0 0 0 0 0\nintr 1 2 3\n");
        assert_eq!(t.len(), 2);
        assert_eq!(t["cpu0"], (98, 82));
        assert_eq!(t["cpu1"], (100, 70));
    }

    #[test]
    fn decisions_match_python_thresholds() {
        assert_eq!(decide(95.0, 50.0, 10, 5, 15), Some(12)); // GPU maxed: +2
        assert_eq!(decide(50.0, 95.0, 14, 5, 15), Some(15)); // core maxed, capped at ceiling
        assert_eq!(decide(60.0, 70.0, 10, 5, 15), Some(9)); // headroom: -1
        assert_eq!(decide(60.0, 70.0, 5, 5, 15), None); // at the floor
        assert_eq!(decide(80.0, 50.0, 10, 5, 15), None); // in between: hold
        assert_eq!(decide(60.0, 85.0, 10, 5, 15), None); // core busy enough to hold
        assert_eq!(decide(95.0, 0.0, 15, 5, 15), None);
    }
}
