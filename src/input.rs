//! Which input you're playing with. Only keyboards and mice are ever opened (by the root service) to see
//! when they were last used. Controllers, including the Deck's built-in one, are NEVER opened: Steam
//! Input owns them, and reading them alongside Steam broke the left stick in at least one game. They
//! count by being connected: keyboard/mouse if used in the last couple of minutes, else a connected
//! controller, else the Deck.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::util::{monotonic, read_or, sysfs};

pub const KEY_A: usize = 30;
pub const KEY_Z: usize = 44;
pub const KEY_SPACE: usize = 57;
const CONTROLLER_NAMES: &[&str] = &["controller", "gamepad", "joystick", "joy-con", "xbox", "x-box", "dualsense", "dualshock", "8bitdo", "stadia"];

#[derive(Clone, Debug, PartialEq)]
pub struct InputDevice {
    pub name: String,
    pub event: String,
    pub kind: String, // "deck", "controller", "kbm"
    pub usb_id: (String, String),
    pub id: String, // "vendor:product"
}

/// Bit n of a /proc/bus/input/devices bitmap (hex longs, most significant first).
pub fn has_bit(words: &[&str], n: usize) -> bool {
    let idx = words.len() as i64 - 1 - (n / 64) as i64;
    if idx < 0 {
        return false;
    }
    match u64::from_str_radix(words[idx as usize], 16) {
        Ok(w) => (w >> (n % 64)) & 1 == 1,
        Err(_) => false,
    }
}

fn field<'a>(block: &'a str, prefix: &str) -> Option<&'a str> {
    block.lines().find_map(|l| l.strip_prefix(prefix))
}

/// `I: Bus=0003 Vendor=28de Product=1205 Version=0111` -> (bus, vendor, product), lowercased.
fn ids(block: &str) -> Option<(String, String, String)> {
    let line = block.lines().find(|l| l.contains("Bus=") && l.contains("Vendor=") && l.contains("Product="))?;
    let grab = |key: &str| -> Option<String> {
        let pos = line.find(key)? + key.len();
        let v: String = line[pos..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        (!v.is_empty()).then(|| v.to_ascii_lowercase())
    };
    Some((grab("Bus=")?, grab("Vendor=")?, grab("Product=")?))
}

fn has_js_handler(handlers: &str) -> bool {
    handlers.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|tok| tok.len() > 2 && tok.starts_with("js") && tok.as_bytes()[2].is_ascii_digit())
}

/// Connected input devices from the text of /proc/bus/input/devices.
pub fn parse_input_devices(text: &str) -> Vec<InputDevice> {
    let mut devices = Vec::new();
    for block in text.split("\n\n") {
        let Some((bus, vendor, product)) = ids(block) else { continue };
        let Some(name) = field(block, "N: Name=\"").and_then(|n| n.rfind('"').map(|e| &n[..e])) else { continue };
        let Some(handlers) = field(block, "H: Handlers=") else { continue };
        let Some(event) = handlers.split_whitespace().find(|t| t.starts_with("event") && t.len() > 5 && t[5..].bytes().all(|b| b.is_ascii_digit())) else { continue };
        let keys_line = field(block, "B: KEY=").unwrap_or("");
        let keys: Vec<&str> = keys_line.split_whitespace().collect();
        let lower = name.to_ascii_lowercase();
        let kind = if lower.contains("virtual") || (vendor.as_str(), product.as_str()) == ("28de", "11ff") {
            None // Steam Input's own virtual pads
        } else if (vendor.as_str(), product.as_str()) == ("28de", "1205") {
            Some("deck") // the Deck's built-in controls
        } else if bus != "0003" && bus != "0005" {
            None // only USB and Bluetooth; skips power button, touchscreen, etc.
        } else if CONTROLLER_NAMES.iter().any(|n| lower.contains(n)) || has_js_handler(handlers) {
            Some("controller")
        } else if handlers.contains("mouse") || [KEY_A, KEY_Z, KEY_SPACE].iter().all(|&k| has_bit(&keys, k)) {
            Some("kbm")
        } else {
            None // headset buttons, media remotes...
        };
        if let Some(kind) = kind {
            devices.push(InputDevice { name: name.to_string(), event: event.to_string(), kind: kind.into(), usb_id: (vendor.clone(), product.clone()), id: format!("{vendor}:{product}") });
        }
    }
    // Controller dongles often add keyboard/mouse interfaces (for macros). Those belong to the controller,
    // so they must never count as, or be opened as, a keyboard or mouse.
    let controller_ids: Vec<(String, String)> = devices.iter().filter(|d| d.kind == "controller").map(|d| d.usb_id.clone()).collect();
    devices.into_iter().filter(|d| !(d.kind == "kbm" && controller_ids.contains(&d.usb_id))).collect()
}

pub fn input_devices() -> Vec<InputDevice> {
    parse_input_devices(&read_or(sysfs("/proc/bus/input/devices"), ""))
}

/// The USB device directory behind /dev/input/eventN (the one with idVendor and power/control).
pub fn usb_device_dir(event: &str) -> Option<PathBuf> {
    let path = std::fs::canonicalize(sysfs(&format!("/sys/class/input/{event}"))).ok()?;
    let mut cur: Option<&std::path::Path> = Some(&path);
    while let Some(p) = cur {
        if p.join("idVendor").exists() && p.join("power").join("control").exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

const RECENT: f64 = 120.0; // seconds an input counts as "in use" after it was last touched
const EVENT_SIZE: usize = 24; // struct input_event on 64-bit: timeval (16) + type u16 + code u16 + value i32
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_HAT0X: u16 = 16;
const ABS_HAT0Y: u16 = 17;

pub struct InputWatcher {
    pub devices: Vec<InputDevice>,
    fds: HashMap<String, (File, InputDevice)>,
    last_used: HashMap<String, (f64, String)>,
    scanned: f64,
}

impl Default for InputWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl InputWatcher {
    pub fn new() -> InputWatcher {
        InputWatcher { devices: Vec::new(), fds: HashMap::new(), last_used: HashMap::new(), scanned: 0.0 }
    }

    pub fn scan(&mut self) {
        self.devices = input_devices();
        let wanted: HashMap<String, InputDevice> = self.devices.iter().filter(|d| d.kind == "kbm").map(|d| (d.event.clone(), d.clone())).collect();
        self.fds.retain(|event, _| wanted.contains_key(event));
        for (event, dev) in wanted {
            if !self.fds.contains_key(&event) {
                let path = sysfs(&format!("/dev/input/{event}"));
                if let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
                    self.fds.insert(event, (f, dev));
                }
                // not root: fall back to which devices are connected
            }
        }
    }

    pub fn poll(&mut self) {
        let now = monotonic();
        if now - self.scanned > 10.0 {
            self.scan();
            self.scanned = now;
        }
        let mut gone = Vec::new();
        let mut buf = [0u8; EVENT_SIZE * 64];
        for (event, (file, dev)) in self.fds.iter_mut() {
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut i = 0;
                        while i + EVENT_SIZE <= n {
                            let e = &buf[i..i + EVENT_SIZE];
                            let etype = u16::from_ne_bytes([e[16], e[17]]);
                            let code = u16::from_ne_bytes([e[18], e[19]]);
                            let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
                            // Button/key presses, mouse movement, d-pad. Stick and trigger noise doesn't count.
                            if (etype == EV_KEY && value == 1) || etype == EV_REL || (etype == EV_ABS && (code == ABS_HAT0X || code == ABS_HAT0Y) && value != 0) {
                                self.last_used.insert(dev.kind.clone(), (now, dev.name.clone()));
                                break;
                            }
                            i += EVENT_SIZE;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        gone.push(event.clone()); // unplugged
                        break;
                    }
                }
            }
        }
        for event in gone {
            self.fds.remove(&event);
        }
    }

    /// (kind, device name, vendor:product).
    pub fn current(&mut self) -> (String, String, String) {
        self.poll();
        let now = monotonic();
        let recent: Vec<(&String, &(f64, String))> = self.last_used.iter().filter(|(_, v)| now - v.0 < RECENT).collect();
        if let Some((kind, (_, name))) = recent.iter().max_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap_or(std::cmp::Ordering::Equal)) {
            let id = self.devices.iter().find(|d| &d.name == name).map(|d| d.id.clone()).unwrap_or_default();
            return ((*kind).clone(), name.clone(), id);
        }
        for kind in ["controller", "kbm"] {
            if let Some(dev) = self.devices.iter().find(|d| d.kind == kind) {
                return (kind.to_string(), dev.name.clone(), dev.id.clone());
            }
        }
        ("deck".into(), "Steam Deck controls".into(), "28de:1205".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES: &str = "I: Bus=0003 Vendor=28de Product=1205 Version=0111\nN: Name=\"Valve Software Steam Deck Controller\"\nP: Phys=usb-0000:04:00.3-3/input2\nH: Handlers=event5 \nB: KEY=1f0000 0 0 0 0\n\nI: Bus=0003 Vendor=28de Product=11ff Version=0001\nN: Name=\"Microsoft X-Box 360 pad 0\"\nH: Handlers=event12 js0\nB: KEY=7cdb000000000000 0 0 0 0\n\nI: Bus=0003 Vendor=3537 Product=1022 Version=0111\nN: Name=\"GameSir-Nova Lite\"\nH: Handlers=event13 js1\nB: KEY=7cdb000000000000 0 0 0 0\n\nI: Bus=0003 Vendor=3537 Product=1022 Version=0111\nN: Name=\"GameSir-Nova Lite Keyboard\"\nH: Handlers=sysrq kbd event14 leds\nB: KEY=1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe\n\nI: Bus=0003 Vendor=046d Product=c52b Version=0111\nN: Name=\"Logitech K400 Plus\"\nH: Handlers=sysrq kbd event15 leds\nB: KEY=1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe\n\nI: Bus=0003 Vendor=046d Product=c52b Version=0111\nN: Name=\"Logitech K400 Plus Mouse\"\nH: Handlers=mouse0 event16\nB: KEY=1f0000 0 0 0 0\n\nI: Bus=0019 Vendor=0000 Product=0001 Version=0000\nN: Name=\"Power Button\"\nH: Handlers=kbd event0\nB: KEY=10000000000000 0\n\nI: Bus=0003 Vendor=1234 Product=5678 Version=0111\nN: Name=\"Steam Virtual Gamepad\"\nH: Handlers=event20 js2\n\nI: Bus=0005 Vendor=054c Product=0ce6 Version=0100\nN: Name=\"DualSense Wireless Controller\"\nH: Handlers=event21 js3\n";

    #[test]
    fn classification() {
        let devs = parse_input_devices(DEVICES);
        let kinds: Vec<(&str, &str, &str)> = devs.iter().map(|d| (d.name.as_str(), d.kind.as_str(), d.event.as_str())).collect();
        assert_eq!(kinds, vec![
            ("Valve Software Steam Deck Controller", "deck", "event5"),
            ("GameSir-Nova Lite", "controller", "event13"),
            ("Logitech K400 Plus", "kbm", "event15"),
            ("Logitech K400 Plus Mouse", "kbm", "event16"),
            ("DualSense Wireless Controller", "controller", "event21"),
        ]);
        assert_eq!(devs[1].id, "3537:1022");
    }

    #[test]
    fn key_bitmap() {
        let words: Vec<&str> = "1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe".split_whitespace().collect();
        assert!(has_bit(&words, KEY_A));
        assert!(has_bit(&words, KEY_Z));
        assert!(has_bit(&words, KEY_SPACE));
        let none: Vec<&str> = "1f0000 0 0 0 0".split_whitespace().collect();
        assert!(!has_bit(&none, KEY_A));
    }
}
