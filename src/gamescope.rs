//! Reads a game's real frame times from gamescope (3.16.20+), the data Steam's overlay is built on.
//!
//! A minimal Wayland client written against the wire protocol directly, so nothing extra needs installing:
//! get the registry, bind gamescope_control (version 6+) as object 4, then send
//! request_app_performance_stats(appid) (opcode 6); gamescope answers with app_performance_stats
//! (event opcode 3: appid, ns_lo, ns_hi) on the game's next frame.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::util::monotonic;

pub const STATS_REQUEST: u16 = 6; // opcodes in gamescope-control.xml
pub const STATS_EVENT: u16 = 3;
pub const CONTROL_OBJECT: u32 = 4;

/// Wayland `string`: u32 length (including NUL), the bytes, NUL, padded to 4 bytes.
pub fn pack_string(text: &str) -> Vec<u8> {
    let mut raw = text.as_bytes().to_vec();
    raw.push(0);
    let mut out = (raw.len() as u32).to_le_bytes().to_vec();
    out.extend(&raw);
    out.resize(out.len() + (4 - raw.len() % 4) % 4, 0);
    out
}

/// (text, offset after the padded string).
pub fn unpack_string(payload: &[u8], offset: usize) -> (String, usize) {
    let length = u32_at(payload, offset) as usize;
    let start = offset + 4;
    let end = (start + length.saturating_sub(1)).min(payload.len());
    let text = String::from_utf8_lossy(&payload[start.min(payload.len())..end]).into_owned();
    (text, start + length + (4 - length % 4) % 4)
}

pub fn u32_at(payload: &[u8], offset: usize) -> u32 {
    if offset + 4 > payload.len() {
        return 0;
    }
    u32::from_le_bytes([payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3]])
}

/// Wire header + payload for one request.
pub fn message(obj: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let size_op = ((8 + payload.len()) as u32) << 16 | opcode as u32;
    let mut out = obj.to_le_bytes().to_vec();
    out.extend(size_op.to_le_bytes());
    out.extend(payload);
    out
}

/// Splits one complete message off the front of `buf`: (object, opcode, payload).
pub fn split_message(buf: &mut Vec<u8>) -> Option<(u32, u16, Vec<u8>)> {
    if buf.len() < 8 {
        return None;
    }
    let obj = u32_at(buf, 0);
    let size_op = u32_at(buf, 4);
    let size = (size_op >> 16) as usize;
    if size < 8 || buf.len() < size {
        return None;
    }
    let payload = buf[8..size].to_vec();
    buf.drain(..size);
    Some((obj, (size_op & 0xFFFF) as u16, payload))
}

/// Parses wl_registry.global (name, interface, version).
pub fn parse_global(payload: &[u8]) -> (u32, String, u32) {
    let name = u32_at(payload, 0);
    let (interface, offset) = unpack_string(payload, 4);
    (name, interface, u32_at(payload, offset))
}

/// The wl_registry.bind request that binds gamescope_control v6 as object 4.
pub fn bind_request(name: u32) -> Vec<u8> {
    let mut payload = name.to_le_bytes().to_vec();
    payload.extend(pack_string("gamescope_control"));
    payload.extend(6u32.to_le_bytes());
    payload.extend(CONTROL_OBJECT.to_le_bytes());
    message(2, 0, &payload)
}

pub fn stats_request(appid: u64) -> Vec<u8> {
    message(CONTROL_OBJECT, STATS_REQUEST, &((appid & 0xFFFF_FFFF) as u32).to_le_bytes())
}

/// app_performance_stats payload -> frame time in ns.
pub fn parse_stats(payload: &[u8]) -> u64 {
    let lo = u32_at(payload, 4) as u64;
    let hi = u32_at(payload, 8) as u64;
    hi << 32 | lo
}

/// Median frame rate from a burst of frame-time samples (ns), or None if nothing was drawn.
pub fn median_fps(mut times: Vec<u64>) -> Option<f64> {
    times.retain(|&t| t > 0);
    times.sort_unstable();
    (!times.is_empty()).then(|| 1e9 / times[times.len() / 2] as f64)
}

pub struct GamescopePerf {
    sock: UnixStream,
    buf: Vec<u8>,
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

impl GamescopePerf {
    pub fn connect(socket_path: &Path) -> Result<GamescopePerf, String> {
        let sock = UnixStream::connect(socket_path).map_err(|e| e.to_string())?;
        sock.set_read_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
        sock.set_write_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
        let mut perf = GamescopePerf { sock, buf: Vec::new() };
        perf.send(&message(1, 1, &2u32.to_le_bytes()))?; // wl_display.get_registry -> registry is object 2
        perf.send(&message(1, 0, &3u32.to_le_bytes()))?; // wl_display.sync -> callback 3 fires once globals are listed
        let mut control: Option<(u32, u32)> = None;
        loop {
            let (obj, opcode, payload) = perf.receive()?;
            if obj == 2 && opcode == 0 {
                let (name, interface, version) = parse_global(&payload);
                if interface == "gamescope_control" {
                    control = Some((name, version));
                }
            } else if obj == 3 {
                break;
            }
        }
        match control {
            Some((name, version)) if version >= 6 => {
                perf.send(&bind_request(name))?;
                Ok(perf)
            }
            _ => Err("this gamescope is too old to report frame times (needs 3.16.20+)".into()),
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.sock.write_all(bytes).map_err(|e| e.to_string())
    }

    /// Next message; Err on protocol error, disconnect or timeout (timeouts carry "timed out").
    fn receive(&mut self) -> Result<(u32, u16, Vec<u8>), String> {
        loop {
            if let Some((obj, opcode, payload)) = split_message(&mut self.buf) {
                if obj == 1 && opcode == 0 {
                    // wl_display.error(object_id, code, message)
                    return Err(format!("gamescope protocol error: {}", unpack_string(&payload, 8).0));
                }
                return Ok((obj, opcode, payload));
            }
            let mut chunk = [0u8; 4096];
            match self.sock.read(&mut chunk) {
                Ok(0) => return Err("gamescope closed the connection".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if is_timeout(&e) => return Err("timed out".into()),
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    /// Average frame rate over a short burst of frame-time samples, or None if the game isn't drawing.
    pub fn fps(&mut self, appid: u64, budget: f64) -> Result<Option<f64>, String> {
        let mut times = Vec::new();
        let deadline = monotonic() + budget;
        'burst: while monotonic() < deadline && times.len() < 20 {
            self.send(&stats_request(appid))?;
            let left = (deadline - monotonic()).max(0.05);
            let _ = self.sock.set_read_timeout(Some(Duration::from_secs_f64(left)));
            loop {
                match self.receive() {
                    Ok((obj, opcode, payload)) => {
                        if obj == CONTROL_OBJECT && opcode == STATS_EVENT {
                            times.push(parse_stats(&payload));
                            break;
                        }
                    }
                    Err(e) if e == "timed out" => break 'burst,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(median_fps(times))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_match_python_struct_packing() {
        // struct.pack("<I", len) + raw + padding
        assert_eq!(pack_string("gamescope_control"), {
            let mut v = 18u32.to_le_bytes().to_vec();
            v.extend(b"gamescope_control\0\0\0");
            v
        });
        assert_eq!(pack_string("abc").len(), 8);
        let (s, off) = unpack_string(&pack_string("wl_compositor"), 0);
        assert_eq!(s, "wl_compositor");
        assert_eq!(off, 4 + 16);
    }

    #[test]
    fn requests_match_python_bytes() {
        // Python: struct.pack("<II", 1, (8 + 4) << 16 | 1) + struct.pack("<I", 2)
        assert_eq!(message(1, 1, &2u32.to_le_bytes()), vec![1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0]);
        assert_eq!(message(1, 0, &3u32.to_le_bytes()), vec![1, 0, 0, 0, 0, 0, 12, 0, 3, 0, 0, 0]);
        // bind: obj 2, opcode 0, payload = name + string + version 6 + new id 4
        let b = bind_request(7);
        assert_eq!(&b[..8], &[2, 0, 0, 0, 0, 0, 44, 0]);
        assert_eq!(&b[8..12], &[7, 0, 0, 0]);
        assert_eq!(&b[36..44], &[6, 0, 0, 0, 4, 0, 0, 0]);
        // request_app_performance_stats(appid) on object 4, opcode 6
        assert_eq!(stats_request(1245620), vec![4, 0, 0, 0, 6, 0, 12, 0, 0xb4, 0x01, 0x13, 0x00]);
        assert_eq!(stats_request(0x1_0000_0001), vec![4, 0, 0, 0, 6, 0, 12, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn events_parse() {
        let mut payload = 3u32.to_le_bytes().to_vec();
        payload.extend(pack_string("gamescope_control"));
        payload.extend(6u32.to_le_bytes());
        let mut buf = message(2, 0, &payload);
        buf.extend(message(3, 0, &5u32.to_le_bytes()));
        let (obj, op, p) = split_message(&mut buf).unwrap();
        assert_eq!((obj, op), (2, 0));
        assert_eq!(parse_global(&p), (3, "gamescope_control".into(), 6));
        let (obj, _, _) = split_message(&mut buf).unwrap();
        assert_eq!(obj, 3);
        assert!(split_message(&mut buf).is_none());
        // app_performance_stats(appid, ns_lo, ns_hi): 16.67ms frame
        let ns: u64 = 16_666_667;
        let mut stats = 1245620u32.to_le_bytes().to_vec();
        stats.extend((ns as u32).to_le_bytes());
        stats.extend(((ns >> 32) as u32).to_le_bytes());
        assert_eq!(parse_stats(&stats), ns);
        let big: u64 = 5_000_000_000;
        let mut stats = 0u32.to_le_bytes().to_vec();
        stats.extend((big as u32).to_le_bytes());
        stats.extend(((big >> 32) as u32).to_le_bytes());
        assert_eq!(parse_stats(&stats), big);
        let f = median_fps(vec![ns, 0, 33_333_333, ns]).unwrap();
        assert!((f - 60.0).abs() < 0.01);
        assert_eq!(median_fps(vec![0, 0]), None);
    }

    #[test]
    fn handshake_against_fake_gamescope() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("deckopt-gs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gamescope-0");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = vec![0u8; 24];
            s.read_exact(&mut got).unwrap();
            assert_eq!(&got[..12], &message(1, 1, &2u32.to_le_bytes())[..]);
            assert_eq!(&got[12..], &message(1, 0, &3u32.to_le_bytes())[..]);
            let mut g = 9u32.to_le_bytes().to_vec();
            g.extend(pack_string("wl_compositor"));
            g.extend(6u32.to_le_bytes());
            s.write_all(&message(2, 0, &g)).unwrap();
            let mut g = 11u32.to_le_bytes().to_vec();
            g.extend(pack_string("gamescope_control"));
            g.extend(6u32.to_le_bytes());
            s.write_all(&message(2, 0, &g)).unwrap();
            s.write_all(&message(3, 0, &1u32.to_le_bytes())).unwrap(); // wl_callback.done
            let mut bind = vec![0u8; 44];
            s.read_exact(&mut bind).unwrap();
            assert_eq!(bind, bind_request(11));
            // two stats requests answered, then silence until the client's budget runs out
            for _ in 0..2 {
                let mut req = vec![0u8; 12];
                s.read_exact(&mut req).unwrap();
                assert_eq!(req, stats_request(1245620));
                let mut p = 1245620u32.to_le_bytes().to_vec();
                p.extend(25_000_000u32.to_le_bytes());
                p.extend(0u32.to_le_bytes());
                s.write_all(&message(4, 3, &p)).unwrap();
            }
            let mut req = vec![0u8; 12];
            let _ = s.read_exact(&mut req);
            std::thread::sleep(Duration::from_millis(600));
        });
        let mut perf = GamescopePerf::connect(&path).unwrap();
        let fps = perf.fps(1245620, 0.4).unwrap().unwrap();
        assert!((fps - 40.0).abs() < 0.01, "{fps}");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_gamescope_is_rejected() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("deckopt-gs-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gamescope-0");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = vec![0u8; 24];
            s.read_exact(&mut got).unwrap();
            let mut g = 11u32.to_le_bytes().to_vec();
            g.extend(pack_string("gamescope_control"));
            g.extend(4u32.to_le_bytes());
            s.write_all(&message(2, 0, &g)).unwrap();
            s.write_all(&message(3, 0, &1u32.to_le_bytes())).unwrap();
        });
        let err = GamescopePerf::connect(&path).err().unwrap();
        assert!(err.contains("too old"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
