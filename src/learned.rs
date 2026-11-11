//! /var/lib/deckopt/learned.json: what AutoTDP and adaptive FPS settled on, per game, power source and
//! screen. Same file and format as the Python version (`json.dumps(indent=1)`), so learned data carries
//! over: `"appid|power|where": watts` and `"fps|appid|power|where": cap`.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::debug;

pub struct Learned {
    pub path: PathBuf,
    pub data: Map<String, Value>,
}

impl Learned {
    pub fn load(path: PathBuf) -> Learned {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        Learned { path, data }
    }

    /// Python's `int(learned.get(key))`: numbers are truncated, anything else is None.
    pub fn get(&self, key: &str) -> Option<i64> {
        match self.data.get(key)? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: i64) {
        self.data.insert(key.to_string(), Value::from(value));
    }

    pub fn save(&self) {
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        if serde::Serialize::serialize(&self.data, &mut ser).is_err() {
            return;
        }
        if let Err(e) = std::fs::write(&self.path, buf) {
            debug!("could not save learned settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_python_format() {
        let dir = std::env::temp_dir().join(format!("deckopt-learned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learned.json");
        // exactly what Python's json.dumps(indent=1) wrote
        std::fs::write(&path, "{\n \"1245620|battery|internal\": 11,\n \"fps|1245620|ac|internal\": 45\n}").unwrap();
        let mut l = Learned::load(path.clone());
        assert_eq!(l.get("1245620|battery|internal"), Some(11));
        assert_eq!(l.get("fps|1245620|ac|internal"), Some(45));
        assert_eq!(l.get("missing"), None);
        l.set("413150|battery|internal", 4);
        l.save();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "{\n \"1245620|battery|internal\": 11,\n \"fps|1245620|ac|internal\": 45,\n \"413150|battery|internal\": 4\n}");
        std::fs::write(&path, "not json").unwrap();
        assert!(Learned::load(path).data.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
