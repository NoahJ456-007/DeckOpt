//! Applies settings and records, per setting, whether it worked (ok), failed, or was skipped.
//! Everything it changes outside the plain tuning knobs (memory tweaks, CPU energy preference, charge
//! limit, USB power saving, VRR/tearing) is remembered and put back by `restore()`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::{setting_str, Config};
use crate::input::{usb_device_dir, InputDevice};
use crate::profile::Final;
use crate::util::{is_digits, list_dir, passwd_entry, proc_dirs, read, strerror, sysfs, which};
use crate::{info, warning};

/// CryoUtilities' recommended memory settings (github.com/CryoByte33/steam-deck-utilities, internal/config.go).
/// The swap file resize it also recommends is left out: that's a one-off disk change, not a runtime setting.
pub const MEMORY_TWEAKS: &[(&str, &str)] = &[
    ("/proc/sys/vm/swappiness", "1"),
    ("/proc/sys/vm/page_lock_unfairness", "1"),
    ("/proc/sys/vm/compaction_proactiveness", "0"),
    ("/sys/kernel/mm/transparent_hugepage/enabled", "always"),
    ("/sys/kernel/mm/transparent_hugepage/shmem_enabled", "advise"),
    ("/sys/kernel/mm/transparent_hugepage/khugepaged/defrag", "0"),
];

pub type Env = IndexMap<String, String>;

/// ok: Some(true) = applied, Some(false) = failed, None = skipped (doesn't apply right now).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub ok: Option<bool>,
    pub detail: String,
}

impl Note {
    pub fn new(ok: Option<bool>, detail: impl Into<String>) -> Note {
        Note { ok, detail: detail.into() }
    }
    pub fn is_ok(&self) -> bool {
        self.ok == Some(true)
    }
}

pub struct Tuner {
    pub user: String,
    pub display: String,
    pub dry_run: bool,
    warned: HashSet<String>,
    pub results: IndexMap<String, Note>,
    pub originals: IndexMap<PathBuf, String>, // path -> value before DeckOpt first changed it, put back on stop
    charge_limit_on: bool,
    awake: HashSet<String>, // controllers whose USB power saving is already handled
    atom_originals: IndexMap<String, (Env, i64)>, // Game Mode switches (VRR, tearing) -> (env, Steam's value)
    pub memory_result: Option<Note>,
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

impl Tuner {
    pub fn new(cfg: &Config, dry_run: bool) -> Tuner {
        Tuner {
            user: setting_str(cfg, "user", "deck"),
            display: setting_str(cfg, "gamescope_display", ":0"),
            dry_run,
            warned: HashSet::new(),
            results: IndexMap::new(),
            originals: IndexMap::new(),
            charge_limit_on: false,
            awake: HashSet::new(),
            atom_originals: IndexMap::new(),
            memory_result: None,
        }
    }

    pub fn warn_once(&mut self, key: &str, msg: String) {
        if self.warned.insert(key.to_string()) {
            warning!("{msg}");
        }
    }

    pub fn note(&mut self, key: &str, ok: Option<bool>, detail: impl Into<String>) {
        self.results.insert(key.to_string(), Note::new(ok, detail));
    }

    pub fn result_ok(&self, key: &str) -> bool {
        self.results.get(key).map(|r| r.is_ok()).unwrap_or(false)
    }

    /// Returns None on success, or the error message.
    pub fn write(&mut self, path: &Path, value: &str) -> Option<String> {
        if self.dry_run {
            info!("[dry-run] {} <- {}", path.display(), value);
            return None;
        }
        match std::fs::write(path, format!("{value}\n")) {
            Ok(()) => None,
            Err(e) => {
                let err = strerror(&e);
                self.warn_once(&path.to_string_lossy(), format!("could not write {}: {}", path.display(), err));
                Some(format!("{}: {}", path.file_name().unwrap_or_default().to_string_lossy(), err))
            }
        }
    }

    pub fn write_restorable(&mut self, path: &Path, value: &str) -> Option<String> {
        if !self.originals.contains_key(path) {
            let Some(current) = read(path) else {
                return Some(format!("{}: not found", path.file_name().unwrap_or_default().to_string_lossy()));
            };
            // "always [madvise] never" style files
            let original = bracketed_word(&current).unwrap_or(current);
            self.originals.insert(path.to_path_buf(), original);
        }
        self.write(path, value)
    }

    pub fn hwmon(&self, names: &[&str]) -> Option<PathBuf> {
        for dir in list_dir(sysfs("/sys/class/hwmon"), |n| n.starts_with("hwmon")) {
            if read(dir.join("name")).map(|n| names.contains(&n.as_str())).unwrap_or(false) {
                return Some(dir);
            }
        }
        None
    }

    pub fn amdgpu_hwmon(&mut self) -> Option<PathBuf> {
        let found = self.hwmon(&["amdgpu"]);
        if found.is_none() {
            self.warn_once("hwmon", "amdgpu hwmon not found; TDP and GPU clock control disabled".into());
        }
        found
    }

    pub fn gpu_device(&self, hwmon: &Path) -> PathBuf {
        let dev = hwmon.join("device");
        if dev.exists() {
            std::fs::canonicalize(&dev).unwrap_or(dev)
        } else {
            hwmon.parent().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| hwmon.to_path_buf())
        }
    }

    pub fn set_tdp(&mut self, watts: i64) {
        let Some(hwmon) = self.amdgpu_hwmon() else {
            return self.note("tdp", Some(false), "AMD GPU power controls not found");
        };
        let caps: Vec<PathBuf> = ["power1_cap", "power2_cap"].iter().map(|c| hwmon.join(c)).filter(|p| p.exists()).collect();
        if caps.is_empty() {
            return self.note("tdp", Some(false), "GPU has no power limit control");
        }
        let errors: Vec<String> = caps.iter().filter_map(|c| self.write(c, &(watts * 1_000_000).to_string())).collect();
        let detail = errors.first().cloned().unwrap_or_else(|| format!("{watts}W"));
        self.note("tdp", Some(errors.is_empty()), detail);
    }

    pub fn set_gpu(&mut self, mhz: &crate::profile::GpuClock) {
        let Some(hwmon) = self.amdgpu_hwmon() else {
            return self.note("gpu", Some(false), "AMD GPU controls not found");
        };
        let device = self.gpu_device(&hwmon);
        let err = match mhz {
            crate::profile::GpuClock::Auto(_) => self.write(&device.join("power_dpm_force_performance_level"), "auto"),
            crate::profile::GpuClock::Mhz(m) => {
                let mut err = self.write(&device.join("power_dpm_force_performance_level"), "manual");
                for cmd in [format!("s 0 {m}"), format!("s 1 {m}"), "c".to_string()] {
                    if err.is_none() {
                        err = self.write(&device.join("pp_od_clk_voltage"), &cmd);
                    }
                }
                err
            }
        };
        let label = match mhz {
            crate::profile::GpuClock::Auto(_) => "auto".to_string(),
            crate::profile::GpuClock::Mhz(m) => format!("{m} MHz"),
        };
        self.note("gpu", Some(err.is_none()), err.unwrap_or(label));
    }

    pub fn set_cpu(&mut self, smt: bool, boost: bool) {
        let onoff = |b: bool| if b { "on" } else { "off" };
        let items: [(&str, &str, String, &str); 2] = [
            ("smt", "/sys/devices/system/cpu/smt/control", onoff(smt).to_string(), onoff(smt)),
            ("cpu_boost", "/sys/devices/system/cpu/cpufreq/boost", (boost as i32).to_string(), onoff(boost)),
        ];
        for (key, path, value, label) in items {
            let path = sysfs(path);
            if path.exists() {
                let err = self.write(&path, &value);
                self.note(key, Some(err.is_none()), err.unwrap_or_else(|| label.to_string()));
            } else {
                self.note(key, None, "not supported on this system");
            }
        }
    }

    /// CPU energy/performance bias (amd-pstate-epp, SteamOS 3.7.5+): "power" ... "performance".
    pub fn set_epp(&mut self, pref: Option<&str>) {
        let files: Vec<PathBuf> = list_dir(sysfs("/sys/devices/system/cpu"), |n| n.starts_with("cpu") && is_digits(&n[3..]))
            .into_iter()
            .map(|d| d.join("cpufreq").join("energy_performance_preference"))
            .filter(|p| p.exists())
            .collect();
        let Some(pref) = pref.filter(|p| !p.is_empty()) else {
            return self.note("cpu_epp", None, "left as is");
        };
        if files.is_empty() {
            return self.note("cpu_epp", None, "not supported by this SteamOS version");
        }
        let available: Vec<String> = read(files[0].with_file_name("energy_performance_available_preferences")).unwrap_or_default().split_whitespace().map(String::from).collect();
        if !available.is_empty() && !available.iter().any(|a| a == pref) {
            return self.note("cpu_epp", Some(false), format!("\"{pref}\" is not one of: {}", available.join(", ")));
        }
        let errors: Vec<String> = files.iter().filter_map(|f| self.write_restorable(f, pref)).collect();
        let detail = errors.first().cloned().unwrap_or_else(|| pref.replace('_', " "));
        self.note("cpu_epp", Some(errors.is_empty()), detail);
    }

    pub fn apply_memory_tweaks(&mut self) {
        let errors: Vec<String> = MEMORY_TWEAKS.iter().filter_map(|(p, v)| self.write_restorable(&sysfs(p), v)).collect();
        if let Some(e) = errors.first() {
            warning!("memory tweaks: {e}");
        }
        self.memory_result = Some(Note::new(Some(errors.is_empty()), errors.first().cloned().unwrap_or_else(|| "CryoUtilities settings".into())));
    }

    /// Ask USB HID gamepads (wired PlayStation and generic pads) for input every 1ms instead of their
    /// default 4-8ms. Xbox-style pads (xpad driver, including GameSir in Xbox mode) already poll fast and
    /// aren't affected. Takes effect for controllers plugged in after this; restored when DeckOpt stops.
    pub fn fast_controller_polling(&mut self) -> Note {
        let path = sysfs("/sys/module/usbhid/parameters/jspoll");
        if !path.exists() {
            return Note::new(None, "not available on this system");
        }
        let err = self.write_restorable(&path, "1");
        Note::new(Some(err.is_none()), err.unwrap_or_else(|| "1000Hz for wired USB pads (re-plug to apply)".into()))
    }

    /// Stops USB power saving on wireless controller dongles: a classic cause of a controller
    /// dropping and reconnecting over and over. The original setting is put back when DeckOpt stops.
    pub fn keep_controllers_awake(&mut self, devices: &[InputDevice]) {
        for dev in devices {
            if dev.kind != "controller" || self.awake.contains(&dev.event) {
                continue;
            }
            self.awake.insert(dev.event.clone());
            let Some(usb) = usb_device_dir(&dev.event) else { continue };
            let control = usb.join("power").join("control");
            if read(&control).as_deref() != Some("on") && self.write_restorable(&control, "on").is_none() {
                info!("keeping {} awake (USB power saving off)", dev.name);
            }
        }
    }

    /// Optional: cap charging while docked to spare the battery; put the user's own limit back after.
    pub fn set_charge_limit(&mut self, docked: bool, limit: i64) {
        let Some(hwmon) = self.hwmon(&["steamdeck_hwmon", "jupiter"]) else { return };
        let path = hwmon.join("max_battery_charge_level");
        if limit == 0 || !path.exists() {
            return;
        }
        if docked && !self.charge_limit_on {
            self.write_restorable(&path, &limit.to_string());
            self.charge_limit_on = true;
            info!("docked: charging capped at {limit}%");
        } else if !docked && self.charge_limit_on {
            if let Some(original) = self.originals.shift_remove(&path) {
                self.write(&path, &original);
            }
            self.charge_limit_on = false;
            info!("undocked: charge limit back to normal");
        }
    }

    // Game Mode (gamescope). The frame cap goes through gamescope's control protocol, the same channel as
    // Steam's own framerate slider: `gamescopectl debug_set_fps_limit N` (gamescope 3.15+, SteamOS 3.6+).
    // Gamescope recomputes the limit from that every frame, so the old GAMESCOPE_FPS_LIMIT /
    // GAMESCOPE_DYNAMIC_REFRESH X properties get overwritten straight away; they're only a fallback for
    // older gamescope versions that don't have gamescopectl.

    /// Environment for talking to the Game Mode session, or None when not in Game Mode.
    pub fn gamescope_env(&self) -> Option<Env> {
        let mut in_game_mode = false;
        let mut steam_env: Option<Env> = None;
        for dir in proc_dirs() {
            let comm = read(dir.join("comm"));
            match comm.as_deref() {
                Some("gamescope") | Some("gamescope-wl") => in_game_mode = true,
                Some("steam") if steam_env.is_none() => {
                    let Ok(raw) = std::fs::read(dir.join("environ")) else { continue };
                    let env: HashMap<String, String> = raw
                        .split(|&b| b == 0)
                        .filter_map(|kv| {
                            let s = String::from_utf8_lossy(kv);
                            s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
                        })
                        .collect();
                    if env.contains_key("DISPLAY") {
                        let mut picked = Env::new();
                        for k in ["DISPLAY", "XAUTHORITY", "GAMESCOPE_WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
                            if let Some(v) = env.get(k) {
                                picked.insert(k.to_string(), v.clone());
                            }
                        }
                        steam_env = Some(picked);
                    }
                }
                _ => {}
            }
        }
        if !in_game_mode {
            return None;
        }
        let mut env = steam_env.unwrap_or_else(|| {
            let mut e = Env::new();
            e.insert("DISPLAY".into(), self.display.clone());
            e
        });
        if !env.contains_key("XDG_RUNTIME_DIR") {
            if let Some((uid, _)) = passwd_entry(&self.user) {
                env.insert("XDG_RUNTIME_DIR".into(), format!("/run/user/{uid}"));
            }
        }
        Some(env)
    }

    /// Runs a command in the user's Game Mode session. Returns (error or None, stdout).
    pub fn run_as_user(&self, env: &Env, cmd: &[&str]) -> (Option<String>, String) {
        if which(cmd[0]).is_none() {
            return (Some(format!("{} is not installed", cmd[0])), String::new());
        }
        let mut full: Vec<String> = vec!["env".into()];
        full.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
        full.extend(cmd.iter().map(|s| s.to_string()));
        if euid() == 0 {
            let mut wrapped = vec!["runuser".to_string(), "-u".into(), self.user.clone(), "--".into()];
            wrapped.extend(full);
            full = wrapped;
        }
        match run_with_timeout(&full, Duration::from_secs(5)) {
            Err(e) => (Some(e), String::new()),
            Ok((code, stdout, stderr)) => {
                if code != 0 {
                    let err = stderr.trim().to_string();
                    (Some(if err.is_empty() { format!("{} exited with {code}", cmd[0]) } else { err }), String::new())
                } else {
                    (None, stdout)
                }
            }
        }
    }

    /// Sets the frame cap (0 = off). Gamescope also switches the built-in screen to the highest refresh
    /// rate that's a multiple of it, exactly like Steam's slider. Returns (error or None, method).
    pub fn send_fps(&mut self, env: &Env, fps: i64) -> (Option<String>, Option<&'static str>) {
        if self.dry_run {
            info!("[dry-run] gamescopectl debug_set_fps_limit {fps}");
            return (None, Some("gamescopectl"));
        }
        let fps_s = fps.to_string();
        let (err, _) = self.run_as_user(env, &["gamescopectl", "debug_set_fps_limit", &fps_s]);
        let Some(err) = err else {
            return (None, Some("gamescopectl"));
        };
        self.warn_once("gamescopectl", format!("gamescopectl failed ({err}), falling back to X properties"));
        let (err2, _) = self.run_as_user(env, &["xprop", "-root", "-f", "GAMESCOPE_FPS_LIMIT", "32c", "-set", "GAMESCOPE_FPS_LIMIT", &fps_s]);
        match err2 {
            None => (None, Some("xprop")),
            Some(e2) => (Some(format!("{err}; {e2}")), None),
        }
    }

    /// Game Mode switches that stay put once set (VRR, tearing), unlike the frame cap. Steam's own
    /// value is remembered and put back when DeckOpt stops.
    pub fn set_display_flag(&mut self, env: &Env, key: &str, atom: &str, value: Option<bool>, on_label: &str) {
        let Some(value) = value else { return };
        let label = if value { on_label.to_string() } else { "off".to_string() };
        if self.dry_run {
            return self.note(key, Some(true), label);
        }
        if !self.atom_originals.contains_key(atom) {
            let (_, out) = self.run_as_user(env, &["xprop", "-root", atom]);
            self.atom_originals.insert(atom.to_string(), (env.clone(), xprop_value(&out).unwrap_or(0)));
        }
        let (err, _) = self.run_as_user(env, &["xprop", "-root", "-f", atom, "32c", "-set", atom, if value { "1" } else { "0" }]);
        self.note(key, Some(err.is_none()), err.unwrap_or(label));
    }

    pub fn apply(&mut self, final_: &Final) {
        self.results.clear();
        self.set_tdp(final_.tdp);
        self.set_gpu(&final_.gpu_mhz);
        self.set_cpu(final_.smt, final_.cpu_boost);
        self.set_epp(final_.cpu_epp.as_deref());
        // FPS cap and refresh live in gamescope, so they only apply in Game Mode.
        let Some(env) = self.gamescope_env() else {
            self.note("fps", None, "only works in Game Mode");
            self.note("refresh", None, "only works in Game Mode");
            return;
        };
        self.set_display_flag(&env, "vrr", "GAMESCOPE_VRR_ENABLED", final_.vrr, "on (if the screen supports it)");
        self.set_display_flag(&env, "allow_tearing", "GAMESCOPE_ALLOW_TEARING", final_.allow_tearing, "allowed (lowest lag)");
        let (err, method) = self.send_fps(&env, final_.fps);
        let fps_detail = if final_.fps != 0 { final_.fps.to_string() } else { "unlimited".to_string() };
        self.note("fps", Some(err.is_none()), err.clone().unwrap_or(fps_detail));
        if err.is_some() {
            self.note("refresh", Some(false), "not changed, the FPS cap couldn't be set");
        } else if let Some(refresh) = final_.refresh {
            if method == Some("gamescopectl") {
                self.note("refresh", Some(true), format!("{refresh}Hz (Game Mode matches it to the FPS cap)"));
            } else {
                // old gamescope: the X property still works there
                let hz = refresh.to_string();
                let (err, _) = self.run_as_user(&env, &["xprop", "-root", "-f", "GAMESCOPE_DYNAMIC_REFRESH", "32c", "-set", "GAMESCOPE_DYNAMIC_REFRESH", &hz]);
                self.note("refresh", Some(err.is_none()), err.unwrap_or_else(|| format!("{refresh}Hz")));
            }
        } else {
            self.note("refresh", None, "external screen sets its own");
        }
    }

    /// Re-asserts the frame cap and lists settings something else (usually Steam) has changed.
    ///
    /// Gamescope has no way to read the active frame cap back, and Steam resends its own (often "off")
    /// when a game starts or its Performance settings are touched, so the cap is simply sent again.
    pub fn verify(&mut self, final_: &Final) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(hwmon) = self.amdgpu_hwmon() {
            if let Some(cap) = read(hwmon.join("power1_cap")).filter(|c| is_digits(c)) {
                let watts = (cap.parse::<f64>().unwrap_or(0.0) / 1_000_000.0).round_ties_even() as i64;
                if watts != final_.tdp {
                    problems.push(format!("Power limit is {watts}W instead of {}W (Steam's TDP limit is probably on)", final_.tdp));
                }
            }
        }
        if let Some(env) = self.gamescope_env() {
            if self.result_ok("fps") {
                let (err, _) = self.send_fps(&env, final_.fps);
                if let Some(err) = err {
                    problems.push(format!("Couldn't re-send the FPS cap: {err}"));
                }
                for (key, atom, value) in [("vrr", "GAMESCOPE_VRR_ENABLED", final_.vrr), ("allow_tearing", "GAMESCOPE_ALLOW_TEARING", final_.allow_tearing)] {
                    if let Some(v) = value {
                        if self.result_ok(key) {
                            self.run_as_user(&env, &["xprop", "-root", "-f", atom, "32c", "-set", atom, if v { "1" } else { "0" }]);
                        }
                    }
                }
            }
        }
        problems
    }

    pub fn restore(&mut self) {
        info!("restoring stock settings");
        self.apply(&Final::stock());
        let originals: Vec<(PathBuf, String)> = self.originals.iter().map(|(p, v)| (p.clone(), v.clone())).collect();
        for (path, value) in originals {
            // memory tweaks, CPU energy preference, charge limit, USB power saving, controller polling
            self.write(&path, &value);
        }
        let atoms: Vec<(String, Env, i64)> = self.atom_originals.iter().map(|(a, (e, v))| (a.clone(), e.clone(), *v)).collect();
        for (atom, env, value) in atoms {
            // VRR, tearing
            self.run_as_user(&env, &["xprop", "-root", "-f", &atom, "32c", "-set", &atom, &value.to_string()]);
        }
    }
}

/// `re.search(r"\[(\w+)\]", text)` -> the bracketed word.
pub fn bracketed_word(text: &str) -> Option<String> {
    for (i, _) in text.match_indices('[') {
        let rest = &text[i + 1..];
        if let Some(end) = rest.find(']') {
            let word = &rest[..end];
            if !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Some(word.to_string());
            }
        }
    }
    None
}

/// `re.search(r"=\s*(\d+)", xprop output)`.
pub fn xprop_value(out: &str) -> Option<i64> {
    let pos = out.find('=')?;
    let rest = out[pos + 1..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        xprop_value(&out[pos + 1..])
    } else {
        digits.parse().ok()
    }
}

/// Runs a command with a timeout. Returns (exit code, stdout, stderr) or an error message.
fn run_with_timeout(argv: &[String], timeout: Duration) -> Result<(i32, String, String), String> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{}: {}", argv[0], strerror(&e)))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("Command '{}' timed out after {} seconds", argv.join(" "), timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracket_and_xprop_parsing() {
        assert_eq!(bracketed_word("always [madvise] never").as_deref(), Some("madvise"));
        assert_eq!(bracketed_word("1"), None);
        assert_eq!(xprop_value("GAMESCOPE_VRR_ENABLED(CARDINAL) = 1\n"), Some(1));
        assert_eq!(xprop_value("GAMESCOPE_VRR_ENABLED:  no such atom on any window.\n"), None);
    }

    #[test]
    fn writes_against_fake_sysroot() {
        let dir = std::env::temp_dir().join(format!("deckopt-tuner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hwmon = dir.join("sys/class/hwmon/hwmon3");
        std::fs::create_dir_all(&hwmon).unwrap();
        std::fs::write(hwmon.join("name"), "amdgpu\n").unwrap();
        std::fs::write(hwmon.join("power1_cap"), "15000000\n").unwrap();
        std::fs::create_dir_all(dir.join("sys/kernel/mm/transparent_hugepage")).unwrap();
        std::fs::write(dir.join("sys/kernel/mm/transparent_hugepage/enabled"), "always [madvise] never\n").unwrap();
        let _guard = crate::util::TEST_LOCK.lock().unwrap();
        std::env::set_var("DECKOPT_SYSROOT", dir.to_string_lossy().to_string());
        let cfg = crate::config::parse("[settings]\nuser = \"deck\"\n").unwrap();
        let mut t = Tuner::new(&cfg, false);
        t.set_tdp(9);
        assert_eq!(t.results["tdp"], Note::new(Some(true), "9W"));
        assert_eq!(read(hwmon.join("power1_cap")).as_deref(), Some("9000000"));
        let thp = sysfs("/sys/kernel/mm/transparent_hugepage/enabled");
        assert_eq!(t.write_restorable(&thp, "always"), None);
        assert_eq!(t.originals[&thp], "madvise");
        assert_eq!(read(&thp).as_deref(), Some("always"));
        assert!(t.write_restorable(&sysfs("/proc/sys/vm/nope"), "1").unwrap().contains("not found"));
        assert_eq!(t.verify(&Final { tdp: 12, ..Final::stock() }), vec!["Power limit is 9W instead of 12W (Steam's TDP limit is probably on)"]);
        std::env::remove_var("DECKOPT_SYSROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
