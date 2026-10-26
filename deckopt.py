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
    return ac or charging, battery


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


def best_refresh(fps, lo, hi):
    """Lowest refresh rate in range that's a whole multiple of the fps cap: even frame pacing, least power."""
    if fps <= 0:
        return hi
    for hz in range(lo, hi + 1):
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
        elif refresh == "max" or fps in ("refresh", "half"):
            hz = hi
        else:  # "fps": match the panel to the frame cap
            hz = best_refresh(int(fps), lo, hi)
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
    def __init__(self, cfg, dry_run=False):
        settings = cfg.get("settings", {})
        self.user = settings.get("user", "deck")
        self.display = settings.get("gamescope_display", ":0")
        self.dry_run = dry_run
        self.warned = set()

    def warn_once(self, key, msg, *args):
        if key not in self.warned:
            self.warned.add(key)
            log.warning(msg, *args)

    def write(self, path, value):
        if self.dry_run:
            log.info("[dry-run] %s <- %s", path, value)
            return True
        try:
            Path(path).write_text(f"{value}\n")
            return True
        except OSError as e:
            self.warn_once(path, "could not write %s: %s", path, e)
            return False

    def amdgpu_hwmon(self):
        for name in glob.glob(sysfs("/sys/class/hwmon/hwmon*/name")):
            if read(name) == "amdgpu":
                return Path(name).parent
        self.warn_once("hwmon", "amdgpu hwmon not found; TDP and GPU clock control disabled")
        return None

    def set_tdp(self, watts):
        hwmon = self.amdgpu_hwmon()
        if hwmon:
            for cap in ("power1_cap", "power2_cap"):
                if (hwmon / cap).exists():
                    self.write(hwmon / cap, watts * 1_000_000)

    def set_gpu(self, mhz):
        hwmon = self.amdgpu_hwmon()
        if not hwmon:
            return
        device = (hwmon / "device").resolve() if (hwmon / "device").exists() else hwmon.parent.parent
        if mhz == "auto":
            self.write(device / "power_dpm_force_performance_level", "auto")
            return
        if self.write(device / "power_dpm_force_performance_level", "manual"):
            for cmd in (f"s 0 {mhz}", f"s 1 {mhz}", "c"):
                self.write(device / "pp_od_clk_voltage", cmd)

    def set_cpu(self, smt, boost):
        smt_ctl = sysfs("/sys/devices/system/cpu/smt/control")
        if os.path.exists(smt_ctl):
            self.write(smt_ctl, "on" if smt else "off")
        boost_ctl = sysfs("/sys/devices/system/cpu/cpufreq/boost")
        if os.path.exists(boost_ctl):
            self.write(boost_ctl, int(boost))

    def xprop(self, atom, value):
        if not shutil.which("xprop"):
            self.warn_once("xprop", "xprop not installed; FPS limit and refresh rate control disabled")
            return
        cmd = ["env", f"DISPLAY={self.display}", "xprop", "-root", "-f", atom, "32c", "-set", atom, str(value)]
        if os.geteuid() == 0:
            cmd = ["runuser", "-u", self.user, "--"] + cmd
        if self.dry_run:
            log.info("[dry-run] %s", " ".join(cmd))
            return
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
            if res.returncode:
                self.warn_once(f"xprop-{atom}", "xprop %s failed: %s", atom, res.stderr.strip())
        except (OSError, subprocess.TimeoutExpired) as e:
            self.warn_once(f"xprop-{atom}", "xprop %s failed: %s", atom, e)

    def apply(self, final):
        self.set_tdp(final["tdp"])
        self.set_gpu(final["gpu_mhz"])
        self.set_cpu(final["smt"], final["cpu_boost"])
        # FPS cap and refresh live in gamescope, so they only apply in Game Mode.
        if process_running("gamescope", "gamescope-wl"):
            self.xprop("GAMESCOPE_FPS_LIMIT", final["fps"])
            if final["refresh"]:
                self.xprop("GAMESCOPE_DYNAMIC_REFRESH", final["refresh"])

    def restore(self):
        log.info("restoring stock settings")
        self.apply({**DEFAULTS, "fps": 0, "refresh": None})


# --- CLI --------------------------------------------------------------------


def load_config(path):
    path = Path(path)
    if not path.exists():
        bundled = Path(__file__).with_name("profiles.toml")
        log.warning("%s not found, using %s", path, bundled)
        path = bundled
    with open(path, "rb") as f:
        return tomllib.load(f), path


def summarize(state, final, trail):
    game = f"{state.game or 'unknown'} ({state.appid})" if state.appid else "none"
    battery = f"{state.battery}%" if state.battery is not None else "?"
    power = "charger" if state.ac else f"battery {battery}" + (" LOW" if state.low_battery else "")
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


def cmd_run(args):
    cfg, path = load_config(args.config)
    mtime = path.stat().st_mtime
    names = GameNames(cfg.get("settings", {}).get("user", "deck"))
    tuner = Tuner(cfg, args.dry_run)
    panel = panel_info()
    log.info("DeckOpt started (%s panel, config %s)", panel["model"], path)

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    last, low, applied_at, settled = None, False, 0.0, True
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
                tuner.apply(final)
                settled, applied_at, last = not changed, now, key
            time.sleep(settings.get("poll_seconds", 3))
    finally:
        tuner.restore()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("command", nargs="?", default="status", choices=["run", "status", "apply"],
                        help="run: background service | status: show what would be applied | apply: apply once")
    parser.add_argument("-c", "--config", default=default_config())
    parser.add_argument("-n", "--dry-run", action="store_true", help="log changes instead of writing them")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO, format="%(levelname)s %(message)s")

    if args.command == "run":
        cmd_run(args)
    else:
        cmd_status(args, apply=args.command == "apply")


if __name__ == "__main__":
    main()
