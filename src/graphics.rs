//! In-game graphics settings (port of the Python `Graphics` / `IniFile`).
//!
//! Some games only run well on the Deck once their own graphics settings are turned down. A
//! `[graphics."<appid>"]` rule names the game's settings file and the values to put in it, with
//! .battery/.ac/.tv/... sub-tables like the system profiles. Games read that file when they start and write it
//! back when they quit, so it is only ever edited while the game is NOT running: right after it quits, when the
//! charger or screen changes with no game open, and when the service starts. What goes in is for the *next*
//! launch, chosen for the state at the time of writing. Only keys the game itself has written are changed;
//! nothing is ever added.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::config::{deep_merge, user, Config};
use crate::detect::{running_appid, State};
use crate::names::vdf_values;
use crate::util::{fmt_g, list_dir, now_epoch, passwd_entry, read_or, strerror, sysfs};
use crate::{info, warning};

// --- Steam paths ---------------------------------------------------------------

/// The Deck user's home. The service runs as root, so it can't just use $HOME.
pub fn user_home(user: &str) -> String {
    passwd_entry(user).map(|(_, home)| home).unwrap_or_else(|| format!("/home/{user}"))
}

/// The user's Steam directory: the native install, the ~/.steam symlink, or the flatpak.
pub fn steam_root(user: &str) -> PathBuf {
    let home = user_home(user);
    for rel in [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/data/Steam"] {
        let path = sysfs(&format!("{home}/{rel}"));
        if path.join("steamapps").is_dir() {
            return path;
        }
    }
    sysfs(&format!("{home}/.local/share/Steam"))
}

/// Every Steam library (internal storage, SD card, ...) from libraryfolders.vdf, the main one first.
pub fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![steam.to_path_buf()];
    let vdf = read_or(steam.join("steamapps").join("libraryfolders.vdf"), "");
    for p in vdf_values(&vdf, "path") {
        let lib = sysfs(&p.replace("\\\\", "\\"));
        if !libraries.contains(&lib) {
            libraries.push(lib);
        }
    }
    libraries
}

/// (library, installdir) of an installed Steam game, from its appmanifest; None if not installed.
pub fn find_app(steam: &Path, appid: &str) -> Option<(PathBuf, Option<String>)> {
    for lib in steam_libraries(steam) {
        if let Ok(raw) = fs::read(lib.join("steamapps").join(format!("appmanifest_{appid}.acf"))) {
            let acf = String::from_utf8_lossy(&raw);
            return Some((lib, vdf_values(&acf, "installdir").into_iter().next()));
        }
    }
    None
}

/// Shell-style `*` / `?` match of one path component (a leading dot must be matched literally, like glob).
fn wildcard(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// Paths under `base` matching a relative pattern whose components may contain `*` / `?`, sorted.
pub fn glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut found = vec![base.to_path_buf()];
    for part in pattern.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for dir in &found {
            if part.contains('*') || part.contains('?') {
                next.extend(list_dir(dir, |name| wildcard(part, name)));
            } else {
                let p = dir.join(part);
                if p.exists() {
                    next.push(p);
                }
            }
        }
        found = next;
    }
    found.sort();
    found
}

// --- Writing files ---------------------------------------------------------------

/// Replaces a file's contents in one step (temp file beside it, then rename) and keeps the owner, group and
/// permissions: the service runs as root, but these are the user's files.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.deckopt", std::process::id()));
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        match std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid())) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {} // not root: it's our own file anyway
            other => other?,
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// --- The ini editor ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Latin1,
}

/// Changes values in an ini file and nothing else: encoding (UTF-8 or UTF-16, with or without a BOM), line
/// endings, key order, spacing, comments and sections all come back byte for byte. Only keys that already
/// exist are touched; which keys exist is the game's business.
pub struct IniFile {
    bom: Vec<u8>,
    encoding: Encoding,
    lines: Vec<(String, String)>, // (content, line ending)
    pub changed: bool,
}

fn decode_utf16(body: &[u8], le: bool) -> Option<String> {
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body.chunks(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
    String::from_utf16(&units).ok()
}

impl IniFile {
    pub fn parse(data: &[u8]) -> IniFile {
        let boms: [(&[u8], Encoding); 3] = [(b"\xef\xbb\xbf", Encoding::Utf8), (b"\xff\xfe", Encoding::Utf16Le), (b"\xfe\xff", Encoding::Utf16Be)];
        let mut bom: Vec<u8> = Vec::new();
        let mut encoding = Encoding::Utf8;
        if let Some((b, e)) = boms.iter().find(|(b, _)| data.starts_with(b)) {
            bom = b.to_vec();
            encoding = *e;
        } else if data.iter().filter(|&&b| b == 0).count() > data.len() / 4 {
            // UTF-16 without a BOM
            encoding = if data.get(1) == Some(&0) { Encoding::Utf16Le } else { Encoding::Utf16Be };
        }
        let body = &data[bom.len()..];
        let decoded = match encoding {
            Encoding::Utf8 => std::str::from_utf8(body).ok().map(str::to_string),
            Encoding::Utf16Le => decode_utf16(body, true),
            Encoding::Utf16Be => decode_utf16(body, false),
            Encoding::Latin1 => None,
        };
        let text = decoded.unwrap_or_else(|| {
            encoding = Encoding::Latin1; // unknown 8-bit encoding: keep every byte as it is
            body.iter().map(|&b| b as char).collect()
        });
        let mut lines = Vec::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
            let content = &rest[..end];
            let after = &rest[end..];
            let ending = if after.starts_with("\r\n") { "\r\n" } else if after.starts_with('\r') { "\r" } else if after.starts_with('\n') { "\n" } else { "" };
            lines.push((content.to_string(), ending.to_string()));
            rest = &after[ending.len()..];
        }
        IniFile { bom, encoding, lines, changed: false }
    }

    /// Section name if the line is `[name]` (surrounding whitespace allowed), lowercased.
    fn section_of(line: &str) -> Option<String> {
        let t = line.trim();
        let inner = t.strip_prefix('[')?.strip_suffix(']')?;
        Some(inner.trim().to_lowercase())
    }

    /// (indent, key, separator, value, trailing whitespace) of a `key = value` line.
    fn split_key(line: &str) -> Option<(&str, &str, &str, &str, &str)> {
        let indent_len = line.len() - line.trim_start().len();
        let (indent, body) = line.split_at(indent_len);
        let first = body.chars().next()?;
        if first.is_whitespace() || "=;#[".contains(first) {
            return None;
        }
        let eq = body.find('=')?;
        let key = body[..eq].trim_end();
        if key.is_empty() {
            return None;
        }
        let after_eq = &body[eq + 1..];
        let value_start = eq + 1 + (after_eq.len() - after_eq.trim_start().len());
        let sep = &body[key.len()..value_start];
        let value_and_trail = &body[value_start..];
        let value = value_and_trail.trim_end();
        let trail = &value_and_trail[value.len()..];
        Some((indent, key, sep, value, trail))
    }

    /// True if the key is in the file (now set to value), false if it isn't (nothing is added).
    pub fn set(&mut self, section: &str, key: &str, value: &str) -> bool {
        let mut current = String::new();
        let want_section = section.to_lowercase();
        let want_key = key.to_lowercase();
        for i in 0..self.lines.len() {
            let content = self.lines[i].0.clone();
            if let Some(s) = Self::section_of(&content) {
                current = s;
                continue;
            }
            if current != want_section {
                continue;
            }
            if let Some((indent, k, sep, v, trail)) = Self::split_key(&content) {
                if k.to_lowercase() == want_key {
                    if v != value {
                        self.lines[i].0 = format!("{indent}{k}{sep}{value}{trail}");
                        self.changed = true;
                    }
                    return true;
                }
            }
        }
        false
    }

    pub fn dump(&self) -> Vec<u8> {
        let text: String = self.lines.iter().map(|(c, e)| format!("{c}{e}")).collect();
        let mut out = self.bom.clone();
        match self.encoding {
            Encoding::Utf8 => out.extend_from_slice(text.as_bytes()),
            Encoding::Utf16Le => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes())),
            Encoding::Utf16Be => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_be_bytes())),
            Encoding::Latin1 => out.extend(text.chars().map(|c| c as u32 as u8)),
        }
        out
    }
}

fn ini_value(v: &Value) -> String {
    match v {
        Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
        Value::Float(f) => fmt_g(*f),
        Value::Integer(i) => i.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// --- Rules ---------------------------------------------------------------

/// Names of the condition sub-tables that apply to this state, most general first ("" = the table itself).
pub fn layer_suffixes(state: &State) -> Vec<String> {
    let kind: Option<&str> = if state.screen.internal { None } else { Some(state.screen.kind.as_str()) };
    let power = state.power();
    let where_ = state.where_();
    vec![
        Some(String::new()),
        Some(power.into()),
        Some(where_.into()),
        kind.map(String::from),
        Some(state.input.clone()),
        Some(format!("{power}+{where_}")),
        kind.map(|k| format!("{power}+{k}")),
        Some(format!("{power}+{}", state.input)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// {section: {key: value}} for this state, layered like the system profiles: the rule's own `values`, then
/// those of .battery/.ac, .internal/.external, .tv/.monitor, .deck/.controller/.kbm, ."ac+tv" and so on.
pub fn graphics_values(rule: &Table, state: &State) -> (IndexMap<String, IndexMap<String, Value>>, Vec<String>) {
    let mut out: IndexMap<String, IndexMap<String, Value>> = IndexMap::new();
    let mut trail = Vec::new();
    for suffix in layer_suffixes(state) {
        let sub = if suffix.is_empty() { Some(rule) } else { rule.get(&suffix).and_then(|v| v.as_table()) };
        let Some(values) = sub.and_then(|s| s.get("values")).and_then(|v| v.as_table()) else { continue };
        for (section, keys) in values {
            if let Some(keys) = keys.as_table() {
                let entry = out.entry(section.clone()).or_default();
                for (k, v) in keys {
                    entry.insert(k.clone(), v.clone());
                }
            }
        }
        trail.push(if suffix.is_empty() { "base".into() } else { suffix });
    }
    (out, trail)
}

/// Game rules (keyed by AppId) with their preset merged in. Non-numeric keys are presets, e.g. `unreal`.
pub fn rules(cfg: &Config) -> IndexMap<String, Table> {
    let mut out = IndexMap::new();
    let Some(tables) = cfg.get("graphics").and_then(|v| v.as_table()) else { return out };
    for (key, rule) in tables {
        let Some(rule) = rule.as_table() else { continue };
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let preset = rule.get("preset").and_then(|p| p.as_str()).and_then(|p| tables.get(p)).and_then(|v| v.as_table());
        out.insert(key.clone(), match preset {
            Some(p) => deep_merge(p, rule),
            None => rule.clone(),
        });
    }
    out
}

pub fn backup_dir() -> PathBuf {
    match std::env::var("DECKOPT_BACKUPS") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::service::app_dir().join("graphics-backup"),
    }
}

/// What happened for one game, shown by `deckopt check`. ok: true applied, None waiting, false failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphicsResult {
    pub game: String,
    pub ok: Option<bool>,
    pub detail: String,
    pub time: f64,
}

pub struct Graphics {
    pub steam: PathBuf,
    pub backups: PathBuf,
    pub dry_run: bool,
    pub results: IndexMap<String, GraphicsResult>,
    warned: HashSet<(String, Vec<String>)>,
}

impl Graphics {
    pub fn new(cfg: &Config, dry_run: bool) -> Graphics {
        Graphics { steam: steam_root(&user(cfg)), backups: backup_dir(), dry_run, results: IndexMap::new(), warned: HashSet::new() }
    }

    /// Applies every rule, unless a game is running (it would overwrite the file on exit anyway).
    pub fn apply_all(&mut self, cfg: &Config, state: &State, name_of: &mut dyn FnMut(&str) -> Option<String>) {
        if running_appid().is_some() {
            return;
        }
        for (appid, rule) in rules(cfg) {
            let game = name_of(&appid).unwrap_or_else(|| format!("game {appid}"));
            self.apply(&appid, &rule, state, &game);
        }
    }

    /// The game's settings file, or Err((ok, why not)).
    pub fn locate(&self, appid: &str, rule: &Table) -> Result<PathBuf, (Option<bool>, String)> {
        let Some((lib, installdir)) = find_app(&self.steam, appid) else { return Err((None, "not installed".into())) };
        let base = if rule.get("root").and_then(|v| v.as_str()) == Some("install") {
            lib.join("steamapps").join("common").join(installdir.unwrap_or_default())
        } else {
            // the Proton prefix's Windows user folder: Documents, AppData, ...
            lib.join("steamapps").join("compatdata").join(appid).join("pfx").join("drive_c").join("users").join("steamuser")
        };
        let patterns: Vec<String> = match rule.get("file") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => Vec::new(),
        };
        let mut found: Vec<PathBuf> = Vec::new();
        for pattern in &patterns {
            for path in glob(&base, pattern) {
                let same = |f: &PathBuf| match (fs::metadata(f), fs::metadata(&path)) {
                    (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                    _ => false,
                };
                if path.is_file() && !found.iter().any(same) {
                    found.push(path);
                }
            }
        }
        match found.len() {
            0 => Err((None, "waiting for the first launch (the game writes its settings file then)".into())),
            1 => Ok(found.remove(0)),
            n => Err((Some(false), format!("{n} settings files match, not sure which to edit"))),
        }
    }

    pub fn apply(&mut self, appid: &str, rule: &Table, state: &State, game: &str) {
        let mut result = GraphicsResult { game: game.to_string(), ok: None, detail: String::new(), time: now_epoch() };
        self.apply_inner(appid, rule, state, game, &mut result);
        self.results.insert(appid.to_string(), result);
    }

    fn apply_inner(&mut self, appid: &str, rule: &Table, state: &State, game: &str, result: &mut GraphicsResult) {
        let path = match self.locate(appid, rule) {
            Ok(p) => p,
            Err((ok, why)) => {
                result.ok = ok;
                result.detail = why;
                return;
            }
        };
        let format = rule.get("format").and_then(|v| v.as_str()).unwrap_or("ini");
        if format != "ini" {
            result.ok = Some(false);
            result.detail = format!("unknown format \"{format}\"");
            return;
        }
        let (values, trail) = graphics_values(rule, state);
        let wanted: Vec<(String, String, String)> = values.iter().flat_map(|(s, keys)| keys.iter().map(move |(k, v)| (s.clone(), k.clone(), ini_value(v)))).collect();
        if wanted.is_empty() {
            result.detail = "nothing to set for this state".into();
            return;
        }
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let outcome = (|| -> io::Result<(IniFile, Vec<String>)> {
            let data = fs::read(&path)?;
            self.backup(appid, &path, &data)?;
            let mut doc = IniFile::parse(&data);
            let skipped: Vec<String> = wanted.iter().filter(|(s, k, v)| !doc.set(s, k, v)).map(|(s, k, _)| if s.is_empty() { k.clone() } else { format!("{s}.{k}") }).collect();
            if doc.changed && !self.dry_run {
                write_atomic(&path, &doc.dump())?;
            }
            Ok((doc, skipped))
        })();
        let (doc, mut skipped) = match outcome {
            Ok(x) => x,
            Err(e) => {
                result.ok = Some(false);
                result.detail = format!("{file_name}: {}", strerror(&e));
                warning!("game settings for {game} not applied: {}", result.detail);
                return;
            }
        };
        let where_ = if state.screen.internal { "built-in screen".to_string() } else { state.screen.kind.clone() };
        let label = format!("{} + {where_}", if state.ac { "charger" } else { "battery" });
        if values.len() == 1 {
            // one section: the key names alone are clearer
            skipped = skipped.into_iter().map(|k| k.split_once('.').map(|(_, b)| b.to_string()).unwrap_or(k)).collect();
        }
        let mut short = skipped.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        if skipped.len() > 3 {
            short += &format!(", +{} more", skipped.len() - 3);
        }
        if skipped.len() == wanted.len() {
            result.ok = Some(false);
            result.detail = format!("none of the keys are in {file_name} ({short})");
        } else {
            result.ok = Some(true);
            result.detail = format!("{} settings for {label}", wanted.len() - skipped.len());
            if !skipped.is_empty() {
                result.detail += &format!("; {} not in the file, skipped ({short})", skipped.len());
            }
        }
        if doc.changed {
            info!("game settings for {game}: {} ({})", result.detail, trail.join(" > "));
        }
        if !skipped.is_empty() && self.warned.insert((appid.to_string(), skipped.clone())) {
            warning!("game settings for {game}: keys not in {file_name}, skipped: {}", skipped.join(", "));
        }
    }

    /// Keeps the untouched original once (first time DeckOpt sees the file), for restore_all().
    fn backup(&self, appid: &str, path: &Path, data: &[u8]) -> io::Result<()> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = self.backups.join(appid).join(&name);
        if dest.exists() || self.dry_run {
            return Ok(());
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::write(&dest, data)?;
        fs::write(self.backups.join(appid).join(format!("{name}.path")), path.to_string_lossy().as_bytes())
    }
}

/// Puts every backed-up settings file back as it was and forgets the backup. Used when DeckOpt is turned off
/// (`deckopt toggle`) and on uninstall, not on every service stop: a restart would otherwise undo the settings
/// for nothing, and shutdown is no time to be writing into the user's files.
/// Returns [(game dir, path, error or None)].
pub fn restore_all(backups: &Path) -> Vec<(String, PathBuf, Option<String>)> {
    let mut done = Vec::new();
    let running = running_appid();
    let mut sides: Vec<PathBuf> = list_dir(backups, |_| true).into_iter().flat_map(|d| list_dir(d, |n| n.ends_with(".path"))).collect();
    sides.sort();
    for side in sides {
        let dir = side.parent().unwrap().to_path_buf();
        let appid = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let side_name = side.file_name().unwrap().to_string_lossy().into_owned();
        let original = dir.join(&side_name[..side_name.len() - ".path".len()]);
        let target = PathBuf::from(read_or(&side, "").trim());
        if running.as_deref() == Some(appid.as_str()) {
            done.push((appid, target, Some("the game is running, try again after quitting".into())));
            continue;
        }
        let r = (|| -> io::Result<()> {
            if target.exists() {
                write_atomic(&target, &fs::read(&original)?)?;
            }
            fs::remove_file(&original)?;
            fs::remove_file(&side)
        })();
        done.push((appid, target, r.err().map(|e| strerror(&e))));
        let _ = fs::remove_dir(&dir);
    }
    done
}

/// `deckopt graphics-restore` (root).
pub fn cmd_restore() -> i32 {
    let done = restore_all(&backup_dir());
    if done.is_empty() {
        println!("No game settings to put back.");
    }
    for (_, path, err) in &done {
        match err {
            Some(e) => println!("FAILED: {} ({e})", path.display()),
            None => println!("restored: {}", path.display()),
        }
    }
    if done.iter().any(|(_, _, e)| e.is_some()) { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{internal, state, tv};

    fn utf16le_bom(text: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xfe];
        text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes()));
        out
    }

    const POE: &str = "; written by the game\r\n[DISPLAY]\r\nshadow_type=High\r\nlight_quality = 2\r\ntexture_quality=TextureQualityHigh\r\nframerate_limit=144\r\n[SOUND]\r\nframerate_limit=1\r\n";

    #[test]
    fn ini_changes_only_existing_values_byte_faithfully() {
        let data = utf16le_bom(POE);
        let mut doc = IniFile::parse(&data);
        assert!(doc.set("display", "SHADOW_TYPE", "Low"));
        assert!(doc.set("DISPLAY", "light_quality", "0"));
        assert!(doc.set("DISPLAY", "framerate_limit", "30"));
        assert!(!doc.set("DISPLAY", "global_illumination_detail", "0"));
        assert!(doc.changed);
        let want = POE.replace("shadow_type=High", "shadow_type=Low").replace("light_quality = 2", "light_quality = 0").replacen("framerate_limit=144", "framerate_limit=30", 1);
        assert_eq!(doc.dump(), utf16le_bom(&want));
        // unchanged document round-trips exactly
        let same = IniFile::parse(&data);
        assert_eq!(same.dump(), data);
    }

    #[test]
    fn ini_encodings_round_trip() {
        for data in [
            b"\xef\xbb\xbf[A]\nk=1\n".to_vec(),
            "[A]\r\nk=1".encode_utf16().flat_map(|u| u.to_le_bytes()).collect(),
            b"[A]\nname=caf\xe9\r\nk=1".to_vec(),
        ] {
            let mut doc = IniFile::parse(&data);
            assert_eq!(doc.dump(), data);
            assert!(doc.set("A", "k", "2"));
            let out = doc.dump();
            assert_eq!(out.len(), data.len());
            assert_ne!(out, data);
        }
    }

    #[test]
    fn wildcard_and_layers() {
        assert!(wildcard("Windows*", "WindowsNoEditor"));
        assert!(!wildcard("*", ".hidden"));
        assert!(wildcard("Game?", "Game1"));
        let s = state(Some("2694490"), None, false, false, internal(), "deck");
        assert_eq!(layer_suffixes(&s), vec!["", "battery", "internal", "deck", "battery+internal", "battery+deck"]);
        let t = state(Some("2694490"), None, true, false, tv(60.0), "controller");
        assert_eq!(layer_suffixes(&t), vec!["", "ac", "external", "tv", "controller", "ac+external", "ac+tv", "ac+controller"]);
    }

    #[test]
    fn locate_apply_backup_restore_on_fake_library() {
        // other tests point DECKOPT_SYSROOT at fixtures, which would redirect the SD-card library path
        let _guard = crate::util::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("deckopt-gfx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let steam = root.join("steam");
        let sd = root.join("sd");
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::create_dir_all(sd.join("steamapps")).unwrap();
        fs::write(steam.join("steamapps/libraryfolders.vdf"), format!("\"libraryfolders\"\n{{\n \"0\" {{ \"path\" \"{}\" }}\n \"1\" {{ \"path\" \"{}\" }}\n}}\n", steam.display(), sd.display())).unwrap();
        fs::write(sd.join("steamapps/appmanifest_2694490.acf"), "\"AppState\"\n{\n \"name\" \"Path of Exile 2\"\n \"installdir\" \"Path of Exile 2\"\n}\n").unwrap();
        let dir = sd.join("steamapps/compatdata/2694490/pfx/drive_c/users/steamuser/Documents/My Games/Path of Exile 2");
        let cfg: Config = toml::from_str(
            r#"
            [graphics."2694490"]
            file = ["Documents/My Games/Path of Exile 2/poe2_production_Config.ini", "My Documents/My Games/Path of Exile 2/poe2_production_Config.ini"]
            [graphics."2694490".values.DISPLAY]
            shadow_type = "Low"
            light_quality = 0
            global_illumination_detail = 0
            [graphics."2694490".battery.values.DISPLAY]
            framerate_limit = 30
            [graphics."2694490".ac.values.DISPLAY]
            framerate_limit = 60
            "#,
        )
        .unwrap();
        let rule = rules(&cfg).shift_remove("2694490").unwrap();
        let mut g = Graphics { steam: steam.clone(), backups: root.join("backup"), dry_run: false, results: IndexMap::new(), warned: HashSet::new() };
        let battery = state(Some("2694490"), None, false, false, internal(), "deck");

        g.apply("2694490", &rule, &battery, "Path of Exile 2");
        assert_eq!(g.results["2694490"].ok, None, "{}", g.results["2694490"].detail);
        assert!(g.results["2694490"].detail.starts_with("waiting"));

        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("poe2_production_Config.ini");
        let original = utf16le_bom(POE);
        fs::write(&file, &original).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        g.apply("2694490", &rule, &battery, "Path of Exile 2");
        let r = &g.results["2694490"];
        assert_eq!(r.ok, Some(true), "{}", r.detail);
        assert_eq!(r.detail, "3 settings for battery + built-in screen; 1 not in the file, skipped (global_illumination_detail)");
        let edited = fs::read(&file).unwrap();
        let want = POE.replace("shadow_type=High", "shadow_type=Low").replace("light_quality = 2", "light_quality = 0").replacen("framerate_limit=144", "framerate_limit=30", 1);
        assert_eq!(edited, utf16le_bom(&want));
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(fs::read(root.join("backup/2694490/poe2_production_Config.ini")).unwrap(), original);

        let ac = state(Some("2694490"), None, true, false, tv(60.0), "controller");
        g.apply("2694490", &rule, &ac, "Path of Exile 2");
        assert!(String::from_utf16_lossy(&fs::read(&file).unwrap()[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).contains("framerate_limit=60"));
        // backup is still the untouched original
        assert_eq!(fs::read(root.join("backup/2694490/poe2_production_Config.ini")).unwrap(), original);

        let done = restore_all(&root.join("backup"));
        assert_eq!(done.len(), 1);
        assert!(done[0].2.is_none(), "{:?}", done[0].2);
        assert_eq!(fs::read(&file).unwrap(), original);
        assert!(!root.join("backup/2694490").exists());
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod bundled_tests {
    use super::*;
    use crate::profile::tests::{internal, state, tv};

    #[test]
    fn shipped_poe2_rule_layers() {
        let cfg = crate::config::parse(crate::config::BUNDLED).unwrap();
        let all = rules(&cfg);
        assert!(all.contains_key("2694490"));
        assert!(all.keys().all(|k| k.chars().all(|c| c.is_ascii_digit())));
        let rule = &all["2694490"];
        let (bat, _) = graphics_values(rule, &state(None, None, false, false, internal(), "deck"));
        let (ac, _) = graphics_values(rule, &state(None, None, true, false, tv(60.0), "controller"));
        let d = |m: &IndexMap<String, IndexMap<String, Value>>, k: &str| ini_value(&m["DISPLAY"][k]);
        assert_eq!(d(&bat, "framerate_limit"), "30");
        assert_eq!(d(&bat, "texture_quality"), "TextureQualityLow");
        assert_eq!(d(&ac, "framerate_limit"), "60");
        assert_eq!(d(&ac, "texture_quality"), "TextureQualityMedium");
        assert_eq!(d(&bat, "use_dynamic_resolution"), "true");
        assert_eq!(d(&ac, "dynamic_resolution_fps"), "60");
    }
}
