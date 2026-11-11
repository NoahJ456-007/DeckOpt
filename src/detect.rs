//! What's going on right now: the game, the charger, the screen, the controllers' batteries.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::{setting_i64, Config};
use crate::input::InputWatcher;
use crate::names::GameNames;
use crate::util::{fmt_g, has_word_ci, is_digits, list_dir, proc_dirs, read, read_or, sysfs};

/// Built-in panel refresh limits per model (DMI product name).
pub struct Panel {
    pub model: &'static str,
    pub refresh: (i64, i64),
}

pub const LCD: Panel = Panel { model: "LCD", refresh: (40, 60) };
pub const OLED: Panel = Panel { model: "OLED", refresh: (45, 90) };

pub fn panel_info() -> Panel {
    match read_or(sysfs("/sys/class/dmi/id/product_name"), "").as_str() {
        "Galileo" => OLED,
        _ => LCD, // "Jupiter" and anything unknown
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub connector: String,
    pub internal: bool,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub refresh: f64,
    pub kind: String, // "built-in", "tv" or "monitor"
}

impl Screen {
    pub fn builtin_fallback() -> Screen {
        Screen { connector: "eDP-1".into(), internal: true, name: "Built-in".into(), width: 0, height: 0, refresh: 0.0, kind: "built-in".into() }
    }

    pub fn describe(&self) -> String {
        let mode = if self.width != 0 { format!(" {}x{}@{}Hz", self.width, self.height, fmt_g(self.refresh)) } else { String::new() };
        let kind = if self.internal { "" } else if self.kind == "tv" { "TV, " } else { "monitor, " };
        format!("{} ({}{}{})", self.name, kind, self.connector, mode)
    }
}

#[derive(Clone, Debug)]
pub struct State {
    pub appid: Option<String>,
    pub game: Option<String>,
    pub ac: bool,
    pub battery: Option<i64>,
    pub low_battery: bool,
    pub screen: Screen,
    pub input: String, // "deck" (built-in controls), "controller" or "kbm"
    pub input_name: String,
    pub input_id: String, // USB/Bluetooth vendor:product of that device, e.g. "3537:1022"
}

impl State {
    pub fn power(&self) -> &'static str {
        if self.ac { "ac" } else { "battery" }
    }
    pub fn where_(&self) -> &'static str {
        if self.screen.internal { "internal" } else { "external" }
    }
    /// What `cmd_run` compares between polls to decide whether to re-apply.
    pub fn key(&self) -> (Option<String>, bool, bool, Screen, String) {
        (self.appid.clone(), self.ac, self.low_battery, self.screen.clone(), self.input.clone())
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct EdidInfo {
    pub name: Option<String>,
    pub width_cm: u8,
    pub cea: bool,
    pub mode: (i64, i64, f64),
}

/// Name, physical width in cm, CEA (TV-style) extension present, and preferred mode (w, h, Hz).
pub fn parse_edid(blob: &[u8]) -> EdidInfo {
    let mut info = EdidInfo::default();
    if blob.len() < 128 || blob[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return info;
    }
    info.width_cm = blob[21];
    info.cea = (128..blob.len()).step_by(128).any(|i| blob[i] == 0x02);
    for off in [54usize, 72, 90, 108] {
        if off + 18 > blob.len() {
            break;
        }
        let d = &blob[off..off + 18];
        let clock = u16::from_le_bytes([d[0], d[1]]) as i64; // in 10 kHz units
        if clock != 0 {
            // Detailed timing descriptor; the first one is the preferred mode.
            if info.mode.0 == 0 {
                let ha = d[2] as i64 | ((d[4] as i64 & 0xF0) << 4);
                let hb = d[3] as i64 | ((d[4] as i64 & 0x0F) << 8);
                let va = d[5] as i64 | ((d[7] as i64 & 0xF0) << 4);
                let vb = d[6] as i64 | ((d[7] as i64 & 0x0F) << 8);
                let total = (ha + hb) * (va + vb);
                let hz = if total > 0 { round2(clock as f64 * 10_000.0 / total as f64) } else { 0.0 };
                info.mode = (ha, va, hz);
            }
        } else if d[3] == 0xFC {
            let text: String = d[5..18].iter().map(|&b| cp437(b)).collect();
            info.name = Some(text.split('\n').next().unwrap_or("").trim().to_string());
        }
    }
    info
}

fn cp437(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else {
        '\u{FFFD}'
    }
}

/// Python's round(x, 2) (ties to even on the scaled value, which is what matters in practice).
pub fn round2(x: f64) -> f64 {
    (x * 100.0).round_ties_even() / 100.0
}

/// TVs: "TV" in the name, 80cm+ wide (~36"+), or no fixed size with HDMI/CEA data (TVs and projectors).
pub fn screen_kind(info: &EdidInfo) -> &'static str {
    if has_word_ci(info.name.as_deref().unwrap_or(""), "TV") || info.width_cm >= 80 {
        return "tv";
    }
    if info.width_cm == 0 && info.cea {
        return "tv";
    }
    "monitor"
}

/// Parses one DRM debugfs `state` dump: connector name -> (w, h, Hz) for connectors with an active CRTC.
pub fn parse_drm_state(text: &str) -> HashMap<String, (i64, i64, f64)> {
    let mut crtc_modes: HashMap<String, Option<(i64, i64, f64)>> = HashMap::new();
    let mut conn_crtc: HashMap<String, String> = HashMap::new();
    let mut section: Option<(String, String)> = None;
    for raw in text.lines() {
        if let Some(sec) = section_header(raw) {
            section = Some(sec);
            continue;
        }
        let Some((kind, name)) = &section else { continue };
        let line = raw.trim();
        if kind == "crtc" {
            if let Some(mode) = parse_mode_line(line) {
                crtc_modes.insert(name.clone(), Some(mode));
            } else if line == "active=0" {
                crtc_modes.insert(name.clone(), None);
            }
        } else if kind == "connector" {
            if let Some(v) = line.strip_prefix("crtc=") {
                conn_crtc.insert(name.clone(), v.to_string());
            }
        }
    }
    let mut modes = HashMap::new();
    for (conn, crtc) in conn_crtc {
        if let Some(Some(mode)) = crtc_modes.get(&crtc) {
            modes.insert(conn, *mode);
        }
    }
    modes
}

/// `crtc[98]: crtc-0` / `connector[106]: eDP-1` at the start of a line.
fn section_header(line: &str) -> Option<(String, String)> {
    for kind in ["crtc", "connector"] {
        if let Some(rest) = line.strip_prefix(kind) {
            let rest = rest.strip_prefix('[')?;
            let close = rest.find(']')?;
            if close == 0 || !rest[..close].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let rest = rest[close + 1..].strip_prefix(": ")?;
            let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if name.is_empty() {
                return None;
            }
            return Some((kind.to_string(), name));
        }
    }
    None
}

/// `mode: "1280x800": 60 71100 1280 1310 1342 1372 800 803 809 822 0x48 0x9`
/// -> (w, h, clock*1000/(htotal*vtotal)), like the Python regex.
fn parse_mode_line(line: &str) -> Option<(i64, i64, f64)> {
    let rest = line.strip_prefix("mode: \"")?;
    let close = rest.find('"')?;
    let name = &rest[..close];
    let (w, h_rest) = name.split_once('x')?;
    if !is_digits(w) {
        return None;
    }
    let w: i64 = w.parse().ok()?;
    let h: String = h_rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let h: i64 = h.parse().ok()?;
    let rest = rest[close + 1..].strip_prefix(": ")?;
    let nums: Vec<i64> = rest.split_whitespace().take(10).map(|t| t.parse::<i64>()).collect::<Result<_, _>>().ok()?;
    if nums.len() < 10 {
        return None;
    }
    let (clock, htotal, vtotal) = (nums[1], nums[5], nums[9]);
    if htotal * vtotal == 0 {
        return None;
    }
    Some((w, h, round2(clock as f64 * 1000.0 / (htotal * vtotal) as f64)))
}

/// Mode each connector is actually driven at, from the DRM debugfs state (root only).
/// The screen's preferred mode isn't always what's in use, e.g. a 144Hz monitor running at 60Hz.
pub fn active_modes() -> HashMap<String, (i64, i64, f64)> {
    let mut modes = HashMap::new();
    for dir in list_dir(sysfs("/sys/kernel/debug/dri"), |_| true) {
        let text = read_or(dir.join("state"), "");
        modes.extend(parse_drm_state(&text));
    }
    modes
}

pub fn screens() -> Vec<Screen> {
    let mut found = Vec::new();
    let live = active_modes();
    for conn_dir in list_dir(sysfs("/sys/class/drm"), |n| n.starts_with("card") && n.contains('-')) {
        if read(conn_dir.join("status")).as_deref() != Some("connected") {
            continue;
        }
        let dirname = conn_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let connector = dirname.split_once('-').map(|(_, c)| c.to_string()).unwrap_or_default();
        let internal = connector.starts_with("eDP") || connector.starts_with("LVDS") || connector.starts_with("DSI");
        let edid = std::fs::read(conn_dir.join("edid")).unwrap_or_default();
        let info = parse_edid(&edid);
        let (w, h, hz) = live.get(&connector).copied().unwrap_or(info.mode);
        let name = if internal { "Built-in".to_string() } else { info.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| connector.clone()) };
        let kind = if internal { "built-in" } else { screen_kind(&info) };
        found.push(Screen { connector, internal, name, width: w, height: h, refresh: hz, kind: kind.into() });
    }
    found
}

/// Gamescope moves the game to an external display whenever one is connected.
pub fn active_screen(found: &[Screen]) -> Screen {
    if let Some(ext) = found.iter().find(|s| !s.internal) {
        return ext.clone();
    }
    found.first().cloned().unwrap_or_else(Screen::builtin_fallback)
}

/// (on wall power, battery percent). No battery at all means wall power.
pub fn power_state() -> (bool, Option<i64>) {
    let (mut ac, mut battery, mut charging) = (false, None, false);
    for dev in list_dir(sysfs("/sys/class/power_supply"), |_| true) {
        let kind = read(dev.join("type"));
        if kind.as_deref() == Some("Mains") {
            ac = ac || read(dev.join("online")).as_deref() == Some("1");
        } else if kind.as_deref() == Some("Battery") && battery.is_none() && read(dev.join("scope")).as_deref() != Some("Device") {
            // not a controller's
            let cap = read_or(dev.join("capacity"), "");
            battery = if is_digits(&cap) { cap.parse().ok() } else { None };
            charging = matches!(read(dev.join("status")).as_deref(), Some("Charging") | Some("Full"));
        }
    }
    (ac || charging || battery.is_none(), battery)
}

/// Steam launches every game under `reaper SteamLaunch AppId=<id> -- ...`.
pub fn running_appid() -> Option<String> {
    for dir in proc_dirs() {
        let Ok(raw) = std::fs::read(dir.join("cmdline")) else { continue };
        let args: Vec<&[u8]> = raw.split(|&b| b == 0).collect();
        if !args.iter().any(|a| *a == b"SteamLaunch") {
            continue;
        }
        for arg in &args {
            if let Some(id) = arg.strip_prefix(b"AppId=") {
                if !id.is_empty() && id.iter().all(|b| b.is_ascii_digit()) {
                    return Some(String::from_utf8_lossy(id).into_owned());
                }
            }
        }
    }
    None
}

#[allow(dead_code)]
pub fn process_running(names: &[&str]) -> bool {
    proc_dirs().iter().any(|d| read(d.join("comm")).map(|c| names.contains(&c.as_str())).unwrap_or(false))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControllerBattery {
    pub name: String,
    pub percent: Option<i64>,
    pub level: String,
    pub charging: bool,
}

/// Batteries of connected controllers: power supplies with scope "Device" (DualShock/DualSense, Xbox
/// over Bluetooth, Switch controllers...).
pub fn controller_batteries() -> Vec<ControllerBattery> {
    let mut found = Vec::new();
    for dev in list_dir(sysfs("/sys/class/power_supply"), |_| true) {
        if read(dev.join("scope")).as_deref() != Some("Device") || read(dev.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let cap = read_or(dev.join("capacity"), "");
        found.push(ControllerBattery {
            name: read(dev.join("model_name")).filter(|n| !n.is_empty()).unwrap_or_else(|| dev.file_name().unwrap_or_default().to_string_lossy().into_owned()),
            percent: if is_digits(&cap) { cap.parse().ok() } else { None },
            level: read_or(dev.join("capacity_level"), ""),
            charging: matches!(read(dev.join("status")).as_deref(), Some("Charging") | Some("Full")),
        });
    }
    found
}

pub fn battery_is_low(b: &ControllerBattery) -> bool {
    !b.charging && (b.percent.map(|p| p <= 20).unwrap_or(false) || b.level == "Low" || b.level == "Critical")
}

pub fn detect(cfg: &Config, names: &mut GameNames, was_low: bool, inputs: &mut InputWatcher) -> State {
    let (ac, battery) = power_state();
    let threshold = setting_i64(cfg, "low_battery_percent", 20);
    // Hysteresis so we don't flap around the threshold.
    let low = !ac && battery.map(|b| b < if was_low { threshold + 5 } else { threshold }).unwrap_or(false);
    let appid = running_appid();
    let (kind, device, device_id) = inputs.current();
    let game = appid.as_deref().and_then(|a| names.get(a));
    State { appid, game, ac, battery, low_battery: low, screen: active_screen(&screens()), input: kind, input_name: device, input_id: device_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_state_parses_live_mode() {
        let text = "plane[31]: plane-0\n\tcrtc=crtc-0\ncrtc[98]: crtc-0\n\tenable=1\n\tactive=1\n\tmode: \"1920x1080\": 60 148500 1920 2008 2052 2200 1080 1084 1089 1125 0x48 0x5\ncrtc[99]: crtc-1\n\tactive=0\nconnector[106]: eDP-1\n\tcrtc=crtc-1\nconnector[110]: DP-1\n\tcrtc=crtc-0\n";
        let modes = parse_drm_state(text);
        assert_eq!(modes.get("DP-1"), Some(&(1920, 1080, 60.0)));
        assert_eq!(modes.get("eDP-1"), None);
    }

    #[test]
    fn drm_mode_line_hz() {
        // Deck OLED panel: 800x1280 @ 90Hz
        let m = parse_mode_line("mode: \"800x1280\": 90 120960 800 816 848 880 1280 1296 1304 1528 0x48 0xa").unwrap();
        assert_eq!(m.0, 800);
        assert_eq!(m.1, 1280);
        assert_eq!(m.2, 89.96); // 120960000 / (880 * 1528) = 89.957...
    }

    fn edid_with(name: &str, width_cm: u8, cea: bool, hz_mode: bool) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        e[21] = width_cm;
        if hz_mode {
            // 1920x1080@60: clock 148.50MHz = 14850 * 10kHz; hblank 280, vblank 45
            let d = &mut e[54..72];
            d[0..2].copy_from_slice(&14850u16.to_le_bytes());
            d[2] = (1920 & 0xFF) as u8;
            d[3] = (280 & 0xFF) as u8;
            d[4] = (((1920 >> 8) & 0xF) << 4) as u8 | ((280 >> 8) & 0xF) as u8;
            d[5] = (1080 & 0xFF) as u8;
            d[6] = 45;
            d[7] = (((1080 >> 8) & 0xF) << 4) as u8;
        }
        let d = &mut e[72..90];
        d[3] = 0xFC;
        let bytes = name.as_bytes();
        for i in 0..13 {
            d[5 + i] = if i < bytes.len() { bytes[i] } else if i == bytes.len() { b'\n' } else { b' ' };
        }
        if cea {
            e.extend(vec![0u8; 128]);
            e[128] = 0x02;
        }
        e
    }

    #[test]
    fn edid_name_mode_and_kind() {
        let info = parse_edid(&edid_with("LG TV SSCR2", 0, true, true));
        assert_eq!(info.name.as_deref(), Some("LG TV SSCR2"));
        assert_eq!(info.mode, (1920, 1080, 60.0));
        assert!(info.cea);
        assert_eq!(screen_kind(&info), "tv");
        let info = parse_edid(&edid_with("DELL S2721DGF", 60, false, false));
        assert_eq!(screen_kind(&info), "monitor");
        let info = parse_edid(&edid_with("BIG", 90, false, false));
        assert_eq!(screen_kind(&info), "tv");
        let info = parse_edid(&edid_with("PROJ", 0, true, false));
        assert_eq!(screen_kind(&info), "tv");
        assert_eq!(parse_edid(b"junk"), EdidInfo::default());
    }

    #[test]
    fn describe_format() {
        let s = Screen { connector: "DP-1".into(), internal: false, name: "LG TV SSCR2".into(), width: 1920, height: 1080, refresh: 60.0, kind: "tv".into() };
        assert_eq!(s.describe(), "LG TV SSCR2 (TV, DP-1 1920x1080@60Hz)");
        let s = Screen { refresh: 59.94, kind: "monitor".into(), ..s };
        assert_eq!(s.describe(), "LG TV SSCR2 (monitor, DP-1 1920x1080@59.94Hz)");
        assert_eq!(Screen::builtin_fallback().describe(), "Built-in (eDP-1)");
    }
}
