//! Small helpers shared by every module: the fake-sysroot prefix, file reads, logging, clocks.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Lets the whole thing run against a fake /sys + /proc tree for testing (DECKOPT_SYSROOT).
pub fn sysfs(path: &str) -> PathBuf {
    match std::env::var("DECKOPT_SYSROOT") {
        Ok(root) if !root.is_empty() => PathBuf::from(format!("{root}{path}")),
        _ => PathBuf::from(path),
    }
}

/// File contents with surrounding whitespace stripped, or None if it can't be read.
pub fn read<P: AsRef<Path>>(path: P) -> Option<String> {
    fs::read(path.as_ref()).ok().map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

pub fn read_or<P: AsRef<Path>>(path: P, default: &str) -> String {
    read(path).unwrap_or_else(|| default.to_string())
}

pub fn clamp(value: i64, lo: i64, hi: i64) -> i64 {
    lo.max(hi.min(value))
}

/// Entries of a directory whose names match `pred`, sorted by name.
pub fn list_dir<P: AsRef<Path>>(dir: P, pred: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir.as_ref()) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| pred(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// /proc/<pid> directories, lowest pid first.
pub fn proc_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<(u64, PathBuf)> = list_dir(sysfs("/proc"), |n| n.bytes().all(|b| b.is_ascii_digit()))
        .into_iter()
        .filter_map(|p| p.file_name()?.to_string_lossy().parse::<u64>().ok().map(|pid| (pid, p)))
        .collect();
    dirs.sort();
    dirs.into_iter().map(|(_, p)| p).collect()
}

pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The OS error text the way Python's `e.strerror` shows it ("Permission denied").
pub fn strerror(e: &io::Error) -> String {
    if let Some(code) = e.raw_os_error() {
        // SAFETY: strerror returns a pointer to a static, NUL-terminated string.
        let s = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) };
        return s.to_string_lossy().into_owned();
    }
    e.to_string()
}

/// Python's `%g`: up to six significant digits, no trailing zeros.
pub fn fmt_g(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e6 {
        return format!("{}", v as i64);
    }
    let digits_before = if v.abs() < 1.0 { 1 } else { (v.abs().log10().floor() as i64) + 1 };
    let prec = (6 - digits_before).max(0) as usize;
    let s = format!("{v:.prec$}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

// --- Logging (Python's `logging.basicConfig(format="%(levelname)s %(message)s")` on stderr) ----

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Tests that point DECKOPT_SYSROOT at a fixture tree take this so they don't run concurrently.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn log_line(level: &str, msg: &str) {
    if level == "DEBUG" && !VERBOSE.load(Ordering::Relaxed) {
        return;
    }
    eprintln!("{level} {msg}");
}

#[macro_export]
macro_rules! info { ($($arg:tt)*) => { $crate::util::log_line("INFO", &format!($($arg)*)) }; }
#[macro_export]
macro_rules! warning { ($($arg:tt)*) => { $crate::util::log_line("WARNING", &format!($($arg)*)) }; }
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { $crate::util::log_line("ERROR", &format!($($arg)*)) }; }
#[macro_export]
macro_rules! debug { ($($arg:tt)*) => { $crate::util::log_line("DEBUG", &format!($($arg)*)) }; }

// --- Clocks ------------------------------------------------------------------------------------

fn clock(id: libc::clockid_t) -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: plain syscall writing into a valid timespec.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Seconds on CLOCK_MONOTONIC (stops while the Deck sleeps), like Python's time.monotonic().
pub fn monotonic() -> f64 {
    clock(libc::CLOCK_MONOTONIC)
}

/// Seconds on CLOCK_BOOTTIME (keeps counting through sleep). None where the clock doesn't exist.
pub fn boottime() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        Some(clock(libc::CLOCK_BOOTTIME))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Unix time in seconds, like Python's time.time().
pub fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Local wall-clock time as "HH:MM".
pub fn local_hhmm() -> String {
    // SAFETY: time(NULL) just returns the clock; localtime_r writes into the tm we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Executable found on PATH, like shutil.which.
pub fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Path::new(cmd).is_file().then(|| PathBuf::from(cmd));
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(cmd);
        if let Ok(meta) = fs::metadata(&candidate) {
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// Home directory of a user from /etc/passwd (uid, home).
pub fn passwd_entry(user: &str) -> Option<(u32, String)> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 6 && f[0] == user {
            return Some((f[2].parse().ok()?, f[5].to_string()));
        }
    }
    None
}

/// Python's `re.search(r"\bTV\b", name, re.I)`.
pub fn has_word_ci(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let word = word.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = lower[start..].find(&word) {
        let i = start + pos;
        let j = i + word.len();
        let before_ok = i == 0 || !is_word(bytes[i - 1]);
        let after_ok = j >= bytes.len() || !is_word(bytes[j]);
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_format() {
        assert_eq!(fmt_g(60.0), "60");
        assert_eq!(fmt_g(59.94), "59.94");
        assert_eq!(fmt_g(143.98), "143.98");
        assert_eq!(fmt_g(0.0), "0");
    }

    #[test]
    fn word_boundary() {
        assert!(has_word_ci("LG TV SSCR2", "TV"));
        assert!(has_word_ci("Samsung tv", "TV"));
        assert!(!has_word_ci("ATVX", "TV"));
        assert!(!has_word_ci("DELL S2721DGF", "TV"));
    }
}
