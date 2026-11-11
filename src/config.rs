//! profiles.toml loading: the bundled profiles (built into the binary, updated with DeckOpt) with the
//! user's file layered on top. Same file format and layer semantics as the Python version.

use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::warning;

/// The profiles shipped with this build. Python read the file next to the script; the binary embeds it
/// so the built-in profiles always match the code.
pub const BUNDLED: &str = include_str!("../profiles.toml");

pub type Config = Table;

pub fn deep_merge(base: &Table, over: &Table) -> Table {
    let mut out = base.clone();
    for (k, v) in over {
        let merged = match (v, out.get(k)) {
            (Value::Table(vt), Some(Value::Table(bt))) => Value::Table(deep_merge(bt, vt)),
            _ => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    out
}

pub fn parse(text: &str) -> Result<Table, toml::de::Error> {
    text.parse::<Table>()
}

/// Bundled profiles with the user's file merged on top. Returns the path to watch for live reloads.
pub fn load_config(path: &Path) -> Result<(Config, PathBuf), toml::de::Error> {
    let cfg = parse(BUNDLED).expect("bundled profiles.toml is valid");
    if !path.exists() {
        warning!("{} not found, using the built-in profiles", path.display());
        return Ok((cfg, path.to_path_buf()));
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let user = parse(&text)?;
    Ok((deep_merge(&cfg, &user), path.to_path_buf()))
}

/// The [settings] table (empty if missing).
pub fn settings(cfg: &Config) -> Table {
    match cfg.get("settings") {
        Some(Value::Table(t)) => t.clone(),
        _ => Table::new(),
    }
}

pub fn setting_i64(cfg: &Config, key: &str, default: i64) -> i64 {
    settings(cfg).get(key).and_then(as_int).unwrap_or(default)
}

pub fn setting_f64(cfg: &Config, key: &str, default: f64) -> f64 {
    match settings(cfg).get(key) {
        Some(Value::Float(f)) => *f,
        Some(Value::Integer(i)) => *i as f64,
        _ => default,
    }
}

pub fn setting_bool(cfg: &Config, key: &str, default: bool) -> bool {
    settings(cfg).get(key).map(as_bool).unwrap_or(default)
}

pub fn setting_str(cfg: &Config, key: &str, default: &str) -> String {
    match settings(cfg).get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => default.to_string(),
    }
}

pub fn user(cfg: &Config) -> String {
    setting_str(cfg, "user", "deck")
}

/// Python's int(value) on a TOML value.
pub fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Float(f) => Some(f.trunc() as i64),
        Value::Boolean(b) => Some(*b as i64),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Python's bool(value) on a TOML value.
pub fn as_bool(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Integer(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Table(t) => !t.is_empty(),
        Value::Datetime(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_profiles_parse() {
        let cfg = parse(BUNDLED).unwrap();
        assert_eq!(setting_i64(&cfg, "poll_seconds", 0), 3);
        assert_eq!(user(&cfg), "deck");
        assert!(setting_bool(&cfg, "memory_tweaks", false));
    }

    #[test]
    fn merge_is_deep_and_keeps_order() {
        let base = parse("[a]\nx = 1\ny = 2\n[a.sub]\nk = 1\n[b]\nz = 1\n").unwrap();
        let over = parse("[a]\ny = 3\n[a.sub]\nj = 2\n[c]\nw = 1\n").unwrap();
        let m = deep_merge(&base, &over);
        assert_eq!(m["a"]["x"].as_integer(), Some(1));
        assert_eq!(m["a"]["y"].as_integer(), Some(3));
        assert_eq!(m["a"]["sub"]["k"].as_integer(), Some(1));
        assert_eq!(m["a"]["sub"]["j"].as_integer(), Some(2));
        assert_eq!(m["c"]["w"].as_integer(), Some(1));
        let keys: Vec<&String> = m.keys().collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }
}
