#!/usr/bin/env python3
"""DeckOpt - per-game Steam Deck tuning that adapts to the charger and the screen.

Watches three things and re-tunes whenever any of them changes:
  * which game is running (Steam AppId)
  * whether the Deck is on the charger (and how low the battery is)
  * which screen you're playing on (built-in panel, or an external monitor/TV)

Settings come from layered profiles in profiles.toml (see that file for the format).
Needs root for the sysfs writes; run it through the bundled systemd service.
"""

import argparse
import glob
import json
import logging
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path

# Lets the whole thing run against a fake /sys + /proc tree for testing.
ROOT = os.environ.get("DECKOPT_SYSROOT", "")

log = logging.getLogger("deckopt")

# Built-in panel refresh limits per model (DMI product name).
PANELS = {
    "Jupiter": {"model": "LCD", "refresh": (40, 60)},
    "Galileo": {"model": "OLED", "refresh": (45, 90)},
}
TDP_RANGE = (3, 15)
GPU_MHZ_RANGE = (200, 1600)
SETTING_KEYS = {"tdp", "gpu_mhz", "fps", "refresh", "smt", "cpu_boost"}
DEFAULTS = {"tdp": 15, "gpu_mhz": "auto", "fps": 0, "refresh": "max", "smt": True, "cpu_boost": True}


def sysfs(path):
    return ROOT + path


def read(path, default=None):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return default


def clamp(value, lo, hi):
    return max(lo, min(hi, int(value)))


# --- Detection -------------------------------------------------------------


@dataclass(frozen=True)
class Screen:
    connector: str
    internal: bool
    name: str
    width: int = 0
    height: int = 0
    refresh: float = 0.0

    def describe(self):
        mode = f" {self.width}x{self.height}@{self.refresh:g}Hz" if self.width else ""
        return f"{self.name} ({self.connector}{mode})"


@dataclass(frozen=True)
class State:
    appid: str | None
    game: str | None
    ac: bool
    battery: int | None
    low_battery: bool
    screen: Screen

    @property
    def power(self):
        return "ac" if self.ac else "battery"

    @property
    def where(self):
        return "internal" if self.screen.internal else "external"


def parse_edid(blob):
    """Return (monitor name, width, height, refresh) of the preferred mode in a base EDID block."""
    if len(blob) < 128 or blob[:8] != b"\x00\xff\xff\xff\xff\xff\xff\x00":
        return None, 0, 0, 0.0
    name, w, h, hz = None, 0, 0, 0.0
    for off in (54, 72, 90, 108):
        d = blob[off:off + 18]
        clock = int.from_bytes(d[0:2], "little")  # in 10 kHz units
        if clock:
            # Detailed timing descriptor; the first one is the preferred mode.
            if not w:
                ha = d[2] | (d[4] & 0xF0) << 4
                hb = d[3] | (d[4] & 0x0F) << 8
                va = d[5] | (d[7] & 0xF0) << 4
                vb = d[6] | (d[7] & 0x0F) << 8
                w, h = ha, va
                hz = round(clock * 10_000 / ((ha + hb) * (va + vb)), 1)
        elif d[3] == 0xFC:
            name = d[5:18].decode("cp437").split("\n")[0].strip()
    return name, w, h, hz


def screens():
    found = []
    for status in sorted(glob.glob(sysfs("/sys/class/drm/card*-*/status"))):
        if read(status) != "connected":
            continue
        conn_dir = Path(status).parent
        connector = conn_dir.name.split("-", 1)[1]
        internal = connector.startswith(("eDP", "LVDS", "DSI"))
        try:
            edid = (conn_dir / "edid").read_bytes()
        except OSError:
            edid = b""
        name, w, h, hz = parse_edid(edid)
        found.append(Screen(connector, internal, "Built-in" if internal else name or connector, w, h, hz))
    return found


def active_screen(found):
    """Gamescope moves the game to an external display whenever one is connected."""
    external = [s for s in found if not s.internal]
    if external:
        return external[0]
    return found[0] if found else Screen("eDP-1", True, "Built-in")


def power_state():
    ac, battery, charging = False, None, False
    for dev in glob.glob(sysfs("/sys/class/power_supply/*")):
        kind = read(dev + "/type")
        if kind == "Mains":
            ac = ac or read(dev + "/online") == "1"
        elif kind == "Battery" and battery is None:
            cap = read(dev + "/capacity", "")
            battery = int(cap) if cap.isdigit() else None
            charging = read(dev + "/status") in ("Charging", "Full")
    # No battery at all means wall power.
    return ac or charging or battery is None, battery


def running_appid():
    """Steam launches every game under `reaper SteamLaunch AppId=<id> -- ...`."""
    for cmdline in glob.glob(sysfs("/proc/[0-9]*/cmdline")):
        try:
            args = Path(cmdline).read_bytes().split(b"\0")
        except OSError:
            continue
        if b"SteamLaunch" not in args:
            continue
        for arg in args:
            m = re.fullmatch(rb"AppId=(\d+)", arg)
            if m:
                return m.group(1).decode()
    return None


def process_running(*names):
    for comm in glob.glob(sysfs("/proc/[0-9]*/comm")):
        if read(comm) in names:
            return True
    return False


def panel_info():
    return PANELS.get(read(sysfs("/sys/class/dmi/id/product_name"), ""), PANELS["Jupiter"])


class GameNames:
    """Looks up game names from Steam's appmanifest files (internal storage and SD card)."""

    def __init__(self, user):
        self.steam = Path(sysfs(f"/home/{user}/.local/share/Steam"))
        self.cache = {}

    def get(self, appid):
        if appid is None:
            return None
        if appid not in self.cache:
            self.cache[appid] = self._lookup(appid)
        return self.cache[appid]

    def _lookup(self, appid):
        libraries = [self.steam]
        vdf = read(self.steam / "steamapps" / "libraryfolders.vdf", "")
        libraries += [Path(sysfs(p)) for p in re.findall(r'"path"\s+"([^"]+)"', vdf)]
        for lib in libraries:
            acf = read(lib / "steamapps" / f"appmanifest_{appid}.acf", "")
            m = re.search(r'"name"\s+"([^"]*)"', acf)
            if m:
                return m.group(1)
        return None


def detect(cfg, names, was_low):
    settings = cfg.get("settings", {})
    ac, battery = power_state()
    threshold = settings.get("low_battery_percent", 20)
    # Hysteresis so we don't flap around the threshold.
    low = not ac and battery is not None and battery < (threshold + 5 if was_low else threshold)
    appid = running_appid()
    return State(appid, names.get(appid), ac, battery, low, active_screen(screens()))


# --- Profile resolution -----------------------------------------------------


def find_table(tables, *keys):
    if not tables:
        return None, None
    lowered = {str(k).lower(): (k, v) for k, v in tables.items()}
    for key in keys:
        if key is not None and str(key).lower() in lowered:
            return lowered[str(key).lower()]
    return None, None


def merge_layer(table, state, label, out, trail):
    """Merge one profile table and its condition sub-tables, most general first."""
    if not table:
        return
    combo = f"{state.power}+{state.where}"
    for suffix, sub in (("", table), (state.power, table.get(state.power)),
                        (state.where, table.get(state.where)), (combo, table.get(combo))):
        if isinstance(sub, dict):
            values = {k: v for k, v in sub.items() if k in SETTING_KEYS}
            if values:
                out.update(values)
                trail.append(f"{label}.{suffix}" if suffix else label)


def resolve(cfg, state):
    out, trail = dict(DEFAULTS), ["defaults"]
    merge_layer(cfg.get("profile"), state, "profile", out, trail)
    key, table = find_table(cfg.get("display"), state.screen.name, state.screen.connector)
    merge_layer(table, state, f'display."{key}"', out, trail)
    game_key, game = find_table(cfg.get("game"), state.appid, state.game)
    merge_layer(game, state, f'game."{game_key}"', out, trail)
    if state.low_battery:
        # Low battery is a safety cap, so it wins over the game profile unless the game sets its own.
        for label, table in (("profile.low_battery", (cfg.get("profile") or {}).get("low_battery")),
                             (f'game."{game_key}".low_battery', (game or {}).get("low_battery"))):
            if isinstance(table, dict):
                out.update({k: v for k, v in table.items() if k in SETTING_KEYS})
                trail.append(label)
    return out, trail


def best_refresh(fps, lo, hi, highest=False):
    """Refresh rate in range that's a whole multiple of the fps cap, for even frame pacing.

    Lowest such rate by default (least power), highest with highest=True (least latency)."""
    if fps <= 0:
        return hi
    rates = range(hi, lo - 1, -1) if highest else range(lo, hi + 1)
    for hz in rates:
        if hz % fps == 0:
            return hz
    return hi


def finalize(settings, state, panel):
    """Turn symbolic values ("refresh", "max", "auto", ...) into concrete numbers for this screen."""
    lo, hi = panel["refresh"]
    fps, refresh = settings["fps"], settings["refresh"]
    if state.screen.internal:
        if isinstance(refresh, int):
            hz = clamp(refresh, lo, hi)
        elif fps in ("refresh", "half"):
            hz = hi
        else:  # "max" or "fps": a rate the frame cap divides evenly, so e.g. 60fps gets 60Hz, not a juddery 90Hz
            hz = best_refresh(int(fps), lo, hi, highest=refresh == "max")
    else:
        hz = round(state.screen.refresh) or 60
    if fps == "refresh":
        fps = hz
    elif fps == "half":
        fps = hz // 2
    gpu = settings["gpu_mhz"]
    return {
        "tdp": clamp(settings["tdp"], *TDP_RANGE),
        "gpu_mhz": "auto" if gpu == "auto" else clamp(gpu, *GPU_MHZ_RANGE),
        "fps": max(0, int(fps)),
        "refresh": hz if state.screen.internal else None,  # gamescope drives external refresh itself
        "smt": bool(settings["smt"]),
        "cpu_boost": bool(settings["cpu_boost"]),
    }


# --- Applying ---------------------------------------------------------------


class Tuner:
    """Applies settings and records, per setting, whether it worked (ok), failed, or was skipped."""

    def __init__(self, cfg, dry_run=False):
        settings = cfg.get("settings", {})
        self.user = settings.get("user", "deck")
        self.display = settings.get("gamescope_display", ":0")
        self.dry_run = dry_run
        self.warned = set()
        self.results = {}

    def warn_once(self, key, msg, *args):
        if key not in self.warned:
            self.warned.add(key)
            log.warning(msg, *args)

    def note(self, key, ok, detail):
        """ok: True = applied, False = failed, None = skipped (doesn't apply right now)."""
        self.results[key] = {"ok": ok, "detail": str(detail)}

    def write(self, path, value):
        """Returns None on success, or the error message."""
        if self.dry_run:
            log.info("[dry-run] %s <- %s", path, value)
            return None
        try:
            Path(path).write_text(f"{value}\n")
            return None
        except OSError as e:
            self.warn_once(path, "could not write %s: %s", path, e)
            return f"{Path(path).name}: {e.strerror or e}"

    def amdgpu_hwmon(self):
        for name in glob.glob(sysfs("/sys/class/hwmon/hwmon*/name")):
            if read(name) == "amdgpu":
                return Path(name).parent
        self.warn_once("hwmon", "amdgpu hwmon not found; TDP and GPU clock control disabled")
        return None

    def gpu_device(self, hwmon):
        return (hwmon / "device").resolve() if (hwmon / "device").exists() else hwmon.parent.parent

    def set_tdp(self, watts):
        hwmon = self.amdgpu_hwmon()
        if not hwmon:
            return self.note("tdp", False, "AMD GPU power controls not found")
        caps = [hwmon / c for c in ("power1_cap", "power2_cap") if (hwmon / c).exists()]
        if not caps:
            return self.note("tdp", False, "GPU has no power limit control")
        errors = [e for e in (self.write(c, watts * 1_000_000) for c in caps) if e]
        self.note("tdp", not errors, errors[0] if errors else f"{watts}W")

    def set_gpu(self, mhz):
        hwmon = self.amdgpu_hwmon()
        if not hwmon:
            return self.note("gpu", False, "AMD GPU controls not found")
        device = self.gpu_device(hwmon)
        if mhz == "auto":
            err = self.write(device / "power_dpm_force_performance_level", "auto")
        else:
            err = self.write(device / "power_dpm_force_performance_level", "manual")
            for cmd in (f"s 0 {mhz}", f"s 1 {mhz}", "c"):
                err = err or self.write(device / "pp_od_clk_voltage", cmd)
        self.note("gpu", not err, err or (f"{mhz} MHz" if mhz != "auto" else "auto"))

    def set_cpu(self, smt, boost):
        for key, path, value, label in (
                ("smt", "/sys/devices/system/cpu/smt/control", "on" if smt else "off", "on" if smt else "off"),
                ("cpu_boost", "/sys/devices/system/cpu/cpufreq/boost", int(boost), "on" if boost else "off")):
            if os.path.exists(sysfs(path)):
                err = self.write(sysfs(path), value)
                self.note(key, not err, err or label)
            else:
                self.note(key, None, "not supported on this system")

    # Gamescope (Game Mode) properties, set on Steam's X display.

    def gamescope_env(self):
        """DISPLAY/XAUTHORITY of the Game Mode session, or None when not in Game Mode."""
        in_game_mode = False
        steam_env = None
        for pid_dir in glob.glob(sysfs("/proc/[0-9]*")):
            comm = read(pid_dir + "/comm")
            if comm in ("gamescope", "gamescope-wl"):
                in_game_mode = True
            elif comm == "steam" and steam_env is None:
                try:
                    raw = Path(pid_dir, "environ").read_bytes().split(b"\0")
                except OSError:
                    continue
                env = dict(kv.decode(errors="replace").split("=", 1) for kv in raw if b"=" in kv)
                if "DISPLAY" in env:
                    steam_env = {k: env[k] for k in ("DISPLAY", "XAUTHORITY") if k in env}
        if not in_game_mode:
            return None
        return steam_env or {"DISPLAY": self.display}

    def xprop(self, env, *args):
        """Runs xprop on the Game Mode display. Returns (error or None, stdout)."""
        if not shutil.which("xprop"):
            return "xprop is not installed", ""
        cmd = ["env"] + [f"{k}={v}" for k, v in env.items()] + ["xprop", "-root", *args]
        if os.geteuid() == 0:
            cmd = ["runuser", "-u", self.user, "--"] + cmd
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
        except (OSError, subprocess.TimeoutExpired) as e:
            return str(e), ""
        if res.returncode:
            return res.stderr.strip() or f"xprop exited with {res.returncode}", ""
        return None, res.stdout

    def set_atom(self, env, key, atom, value, label):
        if self.dry_run:
            log.info("[dry-run] xprop %s = %s on %s", atom, value, env)
            return self.note(key, True, label)
        err, _ = self.xprop(env, "-f", atom, "32c", "-set", atom, str(value))
        if err:
            self.warn_once(f"xprop-{atom}", "setting %s failed: %s", atom, err)
        self.note(key, not err, err or label)

    def get_atom(self, env, atom):
        err, out = self.xprop(env, atom)
        m = re.search(r"=\s*(\d+)", out)
        return int(m.group(1)) if m else None

    def apply(self, final):
        self.results = {}
        self.set_tdp(final["tdp"])
        self.set_gpu(final["gpu_mhz"])
        self.set_cpu(final["smt"], final["cpu_boost"])
        # FPS cap and refresh live in gamescope, so they only apply in Game Mode.
        env = self.gamescope_env()
        if env is None:
            self.note("fps", None, "only works in Game Mode")
            self.note("refresh", None, "only works in Game Mode")
            return
        self.set_atom(env, "fps", "GAMESCOPE_FPS_LIMIT", final["fps"], final["fps"] or "unlimited")
        if final["refresh"]:
            self.set_atom(env, "refresh", "GAMESCOPE_DYNAMIC_REFRESH", final["refresh"], f"{final['refresh']}Hz")
        else:
            self.note("refresh", None, "external screen sets its own")

    def verify(self, final):
        """Reads settings back and lists any that something else (usually Steam) has changed."""
        problems = []
        hwmon = self.amdgpu_hwmon()
        cap = read(hwmon / "power1_cap") if hwmon else None
        if cap and cap.isdigit() and round(int(cap) / 1_000_000) != final["tdp"]:
            problems.append(f"Power limit is {round(int(cap) / 1_000_000)}W instead of {final['tdp']}W "
                            "(Steam's TDP limit is probably on)")
        env = self.gamescope_env()
        if env:
            fps = self.get_atom(env, "GAMESCOPE_FPS_LIMIT")
            if fps is not None and fps != final["fps"]:
                problems.append(f"FPS cap is {fps or 'off'} instead of {final['fps'] or 'off'} "
                                "(Steam's framerate limit is probably on)")
            hz = self.get_atom(env, "GAMESCOPE_DYNAMIC_REFRESH")
            if final["refresh"] and hz is not None and hz != final["refresh"]:
                problems.append(f"Refresh rate is {hz}Hz instead of {final['refresh']}Hz "
                                "(Steam's refresh rate slider was probably changed)")
        return problems

    def restore(self):
        log.info("restoring stock settings")
        self.apply({**DEFAULTS, "fps": 0, "refresh": None})


# --- CLI --------------------------------------------------------------------


def deep_merge(base, over):
    out = dict(base)
    for k, v in over.items():
        out[k] = deep_merge(out[k], v) if isinstance(v, dict) and isinstance(out.get(k), dict) else v
    return out


def load_config(path):
    """The bundled profiles (updated with DeckOpt) with the user's file layered on top."""
    path = Path(path)
    bundled = Path(__file__).with_name("profiles.toml")
    with open(bundled, "rb") as f:
        cfg = tomllib.load(f)
    if not path.exists():
        log.warning("%s not found, using %s", path, bundled)
        return cfg, bundled
    if path.resolve() != bundled.resolve():
        with open(path, "rb") as f:
            cfg = deep_merge(cfg, tomllib.load(f))
    return cfg, path


def power_label(state):
    if state.ac:
        return "charger"
    battery = f"{state.battery}%" if state.battery is not None else "?"
    return f"battery {battery}" + (" LOW" if state.low_battery else "")


def game_label(state):
    return f"{state.game or 'unknown game'} ({state.appid})" if state.appid else None


def summarize(state, final, trail):
    game = game_label(state) or "none"
    power = power_label(state)
    knobs = f"tdp {final['tdp']}W, gpu {final['gpu_mhz']}, fps {final['fps'] or 'unlimited'}"
    if final["refresh"]:
        knobs += f", {final['refresh']}Hz"
    knobs += f", smt {'on' if final['smt'] else 'off'}, boost {'on' if final['cpu_boost'] else 'off'}"
    return (f"game: {game} | power: {power} | screen: {state.screen.describe()}\n"
            f"  -> {knobs}\n  layers: {' > '.join(trail)}")


def default_config():
    return f"/home/{os.environ.get('SUDO_USER') or os.environ.get('USER') or 'deck'}/.config/deckopt/profiles.toml"


def cmd_status(args, apply=False):
    cfg, _ = load_config(args.config)
    state = detect(cfg, GameNames(cfg.get("settings", {}).get("user", "deck")), False)
    settings, trail = resolve(cfg, state)
    final = finalize(settings, state, panel_info())
    print(summarize(state, final, trail))
    if apply:
        Tuner(cfg, args.dry_run).apply(final)


STATE_FILE = Path(os.environ.get("DECKOPT_STATE") or Path(__file__).with_name("state.json"))


def save_state(data):
    """What the service did, for `deckopt check` to read without root."""
    try:
        tmp = STATE_FILE.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=1))
        tmp.chmod(0o644)
        os.replace(tmp, STATE_FILE)
    except OSError as e:
        log.debug("could not save state: %s", e)


def record(status, state, final, tuner, problems):
    status["current"] = {
        "time": time.time(), "game": game_label(state), "power": power_label(state),
        "screen": state.screen.describe(), "target": final, "results": dict(tuner.results),
        "problems": problems, "game_mode": tuner.results.get("fps", {}).get("ok") is not None,
    }
    if state.appid:
        status["last_game"] = status["current"]
    save_state(status)


def cmd_run(args):
    cfg, path = load_config(args.config)
    mtime = path.stat().st_mtime
    names = GameNames(cfg.get("settings", {}).get("user", "deck"))
    tuner = Tuner(cfg, args.dry_run)
    panel = panel_info()
    log.info("DeckOpt started (%s panel, config %s)", panel["model"], path)
    status = {"version": read(Path(__file__).with_name("VERSION"), "dev"), "started": time.time(),
              "current": None, "last_game": None, "events": []}

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    last, low, applied_at, settled, verified_at = None, False, 0.0, True, 0.0
    state = final = None
    try:
        while True:
            settings = cfg.get("settings", {})
            if path.exists() and path.stat().st_mtime != mtime:
                try:
                    cfg, path = load_config(path)
                    mtime = path.stat().st_mtime
                    last = None  # force a re-apply with the new profiles
                    log.info("config reloaded")
                except tomllib.TOMLDecodeError as e:
                    mtime = path.stat().st_mtime
                    log.error("config has an error, keeping the old one: %s", e)

            state = detect(cfg, names, low)
            low = state.low_battery
            key = (state.appid, state.ac, state.low_battery, state.screen)
            now = time.monotonic()
            # Steam applies its own per-game settings a few seconds after launch, so apply once more after that.
            resettle = not settled and now - applied_at >= settings.get("settle_seconds", 8)
            if key != last or resettle:
                values, trail = resolve(cfg, state)
                final = finalize(values, state, panel)
                changed = key != last
                if changed:
                    log.info(summarize(state, final, trail))
                    knobs = f"{final['tdp']}W, {final['fps'] or 'unlimited'} FPS"
                    status["events"] = (status["events"] + [
                        f"{time.strftime('%H:%M')}  {game_label(state) or 'no game'} | {power_label(state)} | "
                        f"{state.screen.name} -> {knobs}"])[-8:]
                tuner.apply(final)
                for key_name, r in tuner.results.items():
                    if r["ok"] is False:
                        log.warning("%s not applied: %s", key_name, r["detail"])
                settled, applied_at, last = not changed, now, key
                verified_at = 0.0 if resettle else now  # check right after the settle pass
                record(status, state, final, tuner, [])
            # Watch for Steam (or anything else) changing our settings afterwards.
            if settled and now - verified_at >= settings.get("verify_seconds", 15):
                verified_at = now
                problems = [] if args.dry_run else tuner.verify(final)
                if problems != status["current"]["problems"]:
                    for p in problems:
                        log.warning(p)
                record(status, state, final, tuner, problems)
            time.sleep(settings.get("poll_seconds", 3))
    finally:
        tuner.restore()


def ago(ts):
    mins = int((time.time() - ts) / 60)
    if mins < 1:
        return "just now"
    if mins < 60:
        return f"{mins} min ago"
    return f"{mins // 60} h {mins % 60} min ago"


def cmd_check(args):
    """Plain-English health check: short enough to fit on one screen for a photo."""
    cfg, _ = load_config(args.config)
    tty = sys.stdout.isatty()
    color = (lambda code, t: f"\033[{code}m{t}\033[0m") if tty else (lambda code, t: t)
    marks = {True: color(32, "OK "), False: color(31, "BAD"), None: color(90, " - ")}

    def line(ok, text):
        print(f" {marks[ok]}  {text}")

    try:
        data = json.loads(STATE_FILE.read_text())
    except (OSError, ValueError):
        data = None
    print(color(1, f"DeckOpt check  (version {(data or {}).get('version') or read(Path(__file__).with_name('VERSION'), 'dev')})"))
    print()

    try:
        active = subprocess.run(["systemctl", "is-active", "deckopt"], capture_output=True, text=True).stdout.strip()
    except OSError:
        active = "unknown"
    line(active == "active", "Background service is running" if active == "active"
         else f"Background service is {active or 'not installed'}. Fix: deckopt restart")

    state = detect(cfg, GameNames(cfg.get("settings", {}).get("user", "deck")), False)
    line(None, f"Right now: {game_label(state) or 'no game running'}, {power_label(state)}, {state.screen.describe()}")

    if not data or not data.get("current"):
        line(False, "The service hasn't recorded anything yet. Fix: deckopt restart, wait 20 seconds, check again")
        return
    rec = data.get("last_game") or data["current"]
    print()
    where = "in Game Mode" if rec["game_mode"] else "in Desktop Mode"
    print(color(1, f"{'While playing ' + rec['game'] if rec['game'] else 'Last update'} ({ago(rec['time'])}, {where}):"))
    print(f"      {rec['power']}, {rec['screen']}")
    labels = [("tdp", "Power limit"), ("gpu", "GPU clock"), ("fps", "FPS cap"), ("refresh", "Refresh rate"),
              ("smt", "Hyperthreading"), ("cpu_boost", "CPU boost")]
    for key, label in labels:
        r = rec["results"].get(key)
        if r:
            line(r["ok"], f"{label}: {r['detail']}")
    for p in rec["problems"]:
        line(False, p)
    if rec["game"] and not rec["game_mode"]:
        line(False, "Game Mode wasn't detected, so the FPS cap and refresh rate were skipped")
    if not data.get("last_game"):
        line(None, "No game has been played since the service started")

    if data.get("events"):
        print()
        print(color(1, "Recent changes:"))
        for e in data["events"][-6:]:
            print(f"      {e}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("command", nargs="?", default="status", choices=["run", "status", "apply", "check"],
                        help="run: background service | status: show what would be applied | apply: apply once | check: health check")
    parser.add_argument("-c", "--config", default=default_config())
    parser.add_argument("-n", "--dry-run", action="store_true", help="log changes instead of writing them")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO, format="%(levelname)s %(message)s")

    if args.command == "run":
        cmd_run(args)
    elif args.command == "check":
        cmd_check(args)
    else:
        cmd_status(args, apply=args.command == "apply")


if __name__ == "__main__":
    main()
