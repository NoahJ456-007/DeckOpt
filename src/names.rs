//! Game names from Steam's appmanifest files (internal storage and SD card) and, for non-Steam games
//! (emulators, EmuDeck entries...), the binary userdata/*/config/shortcuts.vdf.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::util::{list_dir, read_or};

pub struct GameNames {
    steam: PathBuf,
    cache: HashMap<String, Option<String>>,
}

impl GameNames {
    pub fn new(user: &str) -> GameNames {
        GameNames { steam: crate::graphics::steam_root(user), cache: HashMap::new() }
    }

    pub fn get(&mut self, appid: &str) -> Option<String> {
        if !self.cache.contains_key(appid) {
            let name = self.lookup(appid);
            self.cache.insert(appid.to_string(), name);
        }
        self.cache[appid].clone()
    }

    fn lookup(&self, appid: &str) -> Option<String> {
        for lib in crate::graphics::steam_libraries(&self.steam) {
            let acf = read_or(lib.join("steamapps").join(format!("appmanifest_{appid}.acf")), "");
            if let Some(name) = vdf_values(&acf, "name").into_iter().next() {
                return Some(name);
            }
        }
        let id: u64 = appid.parse().ok()?;
        for user_dir in list_dir(self.steam.join("userdata"), |_| true) {
            let Ok(data) = std::fs::read(user_dir.join("config").join("shortcuts.vdf")) else { continue };
            if let Some(name) = shortcut_name(&data, id) {
                return Some(name);
            }
        }
        None
    }
}

/// Every `"key"   "value"` in a text VDF, like re.findall(r'"key"\s+"([^"]*)"').
pub fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(&needle) {
        let after = &rest[pos + needle.len()..];
        let trimmed = after.trim_start();
        if trimmed.len() < after.len() {
            if let Some(v) = trimmed.strip_prefix('"') {
                if let Some(end) = v.find('"') {
                    out.push(v[..end].to_string());
                    rest = &v[end + 1..];
                    continue;
                }
            }
        }
        rest = &rest[pos + needle.len()..];
    }
    out
}

fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Binary shortcuts.vdf: `\x02appid\x00<u32 LE>` ... `\x01appname\x00<name>\x00` (case-insensitive keys).
pub fn shortcut_name(data: &[u8], appid: u64) -> Option<String> {
    let mut from = 0;
    while let Some(i) = find_ci(data, b"\x02appid\x00", from) {
        let end = i + 7;
        if end + 4 > data.len() {
            break;
        }
        let id = u32::from_le_bytes([data[end], data[end + 1], data[end + 2], data[end + 3]]) as u64;
        from = end + 4;
        if id != appid {
            continue;
        }
        if let Some(j) = find_ci(data, b"\x01appname\x00", from) {
            let start = j + 9;
            let stop = data[start..].iter().position(|&b| b == 0).map(|p| start + p)?;
            return Some(String::from_utf8_lossy(&data[start..stop]).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_vdf() {
        let lib = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/deck/.local/share/Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/run/media/mmcblk0p1\"\n\t}\n}\n";
        assert_eq!(vdf_values(lib, "path"), vec!["/home/deck/.local/share/Steam", "/run/media/mmcblk0p1"]);
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"1145360\"\n\t\"name\"\t\t\"Hades\"\n}\n";
        assert_eq!(vdf_values(acf, "name"), vec!["Hades"]);
        assert!(vdf_values("\"name\"\"nospace\"", "name").is_empty());
    }

    fn shortcuts(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut d = b"\x00shortcuts\x00".to_vec();
        for (n, (id, name)) in entries.iter().enumerate() {
            d.extend(format!("\x00{n}\x00").as_bytes());
            d.extend(b"\x02appid\x00");
            d.extend(id.to_le_bytes());
            d.extend(b"\x01AppName\x00");
            d.extend(name.as_bytes());
            d.push(0);
            d.extend(b"\x01Exe\x00\"/usr/bin/foo\"\x00\x08\x08");
        }
        d
    }

    #[test]
    fn binary_shortcuts() {
        let data = shortcuts(&[(2870985116, "RetroArch"), (4027034257, "Dolphin")]);
        assert_eq!(shortcut_name(&data, 4027034257).as_deref(), Some("Dolphin"));
        assert_eq!(shortcut_name(&data, 2870985116).as_deref(), Some("RetroArch"));
        assert_eq!(shortcut_name(&data, 42), None);
    }
}
