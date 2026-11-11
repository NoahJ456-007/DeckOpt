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
import codecs
import glob
import json
import logging
import os
import pwd
import re
import shutil
import signal
import socket
import stat
import struct
import subprocess
import sys
import tempfile
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
SETTING_KEYS = {"tdp", "auto_tdp", "gpu_mhz", "fps", "adaptive_fps", "max_fps", "refresh", "smt", "cpu_boost",
                "cpu_epp", "vrr", "allow_tearing"}
DEFAULTS = {"tdp": 15, "auto_tdp": False, "gpu_mhz": "auto", "fps": 0, "adaptive_fps": False, "max_fps": 60,
            "refresh": "max", "smt": True, "cpu_boost": True, "cpu_epp": None, "vrr": None, "allow_tearing": None}

# CryoUtilities' recommended memory settings (github.com/CryoByte33/steam-deck-utilities, internal/config.go).
# The swap file resize it also recommends is left out: that's a one-off disk change, not a runtime setting.
MEMORY_TWEAKS = {
    "/proc/sys/vm/swappiness": "1",
    "/proc/sys/vm/page_lock_unfairness": "1",
    "/proc/sys/vm/compaction_proactiveness": "0",
    "/sys/kernel/mm/transparent_hugepage/enabled": "always",
    "/sys/kernel/mm/transparent_hugepage/shmem_enabled": "advise",
    "/sys/kernel/mm/transparent_hugepage/khugepaged/defrag": "0",
}


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
    kind: str = "built-in"  # "built-in", "tv" or "monitor"

    def describe(self):
        mode = f" {self.width}x{self.height}@{self.refresh:g}Hz" if self.width else ""
        kind = "" if self.internal else ("TV, " if self.kind == "tv" else "monitor, ")
        return f"{self.name} ({kind}{self.connector}{mode})"


@dataclass(frozen=True)
class State:
    appid: str | None
    game: str | None
    ac: bool
    battery: int | None
    low_battery: bool
    screen: Screen
    input: str = "deck"          # "deck" (built-in controls), "controller" or "kbm"
    input_name: str = ""
    input_id: str = ""           # USB/Bluetooth vendor:product of that device, e.g. "3537:1022"

    @property
    def power(self):
        return "ac" if self.ac else "battery"

    @property
    def where(self):
        return "internal" if self.screen.internal else "external"


def parse_edid(blob):
    """Name, physical width in cm, CEA (TV-style) extension present, and preferred mode (w, h, Hz)."""
    info = {"name": None, "width_cm": 0, "cea": False, "mode": (0, 0, 0.0)}
    if len(blob) < 128 or blob[:8] != b"\x00\xff\xff\xff\xff\xff\xff\x00":
        return info
    info["width_cm"] = blob[21]
    info["cea"] = any(blob[i] == 0x02 for i in range(128, len(blob), 128))
    for off in (54, 72, 90, 108):
        d = blob[off:off + 18]
        clock = int.from_bytes(d[0:2], "little")  # in 10 kHz units
        if clock:
            # Detailed timing descriptor; the first one is the preferred mode.
            if not info["mode"][0]:
                ha = d[2] | (d[4] & 0xF0) << 4
                hb = d[3] | (d[4] & 0x0F) << 8
                va = d[5] | (d[7] & 0xF0) << 4
                vb = d[6] | (d[7] & 0x0F) << 8
                info["mode"] = (ha, va, round(clock * 10_000 / ((ha + hb) * (va + vb)), 2))
        elif d[3] == 0xFC:
            info["name"] = d[5:18].decode("cp437").split("\n")[0].strip()
    return info


def screen_kind(info):
    """TVs: "TV" in the name, 80cm+ wide (~36"+), or no fixed size with HDMI/CEA data (TVs and projectors)."""
    if re.search(r"\bTV\b", info["name"] or "", re.I) or info["width_cm"] >= 80:
        return "tv"
    if info["width_cm"] == 0 and info["cea"]:
        return "tv"
    return "monitor"


def active_modes():
    """Mode each connector is actually driven at, from the DRM debugfs state (root only).

    The screen's preferred mode isn't always what's in use, e.g. a 144Hz monitor running at 60Hz."""
    modes = {}
    for state_file in glob.glob(sysfs("/sys/kernel/debug/dri/*/state")):
        text = read(state_file, "")
        crtc_modes, conn_crtc, section = {}, {}, None
        for line in text.splitlines():
            m = re.match(r"(crtc|connector)\[\d+\]: (\S+)", line)
            if m:
                section = m.groups()
                continue
            if not section:
                continue
            line = line.strip()
            if section[0] == "crtc":
                m = re.match(r'mode: "(\d+)x(\d+)[^"]*": \d+ (\d+) \d+ \d+ \d+ (\d+) \d+ \d+ \d+ (\d+)', line)
                if m:
                    w, h, clock, htotal, vtotal = map(int, m.groups())
                    crtc_modes[section[1]] = (w, h, round(clock * 1000 / (htotal * vtotal), 2))
                elif line == "active=0":
                    crtc_modes[section[1]] = None
            elif section[0] == "connector" and line.startswith("crtc="):
                conn_crtc[section[1]] = line.split("=", 1)[1]
        for conn, crtc in conn_crtc.items():
            if crtc_modes.get(crtc):
                modes[conn] = crtc_modes[crtc]
    return modes


def screens():
    found = []
    live = active_modes()
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
        info = parse_edid(edid)
        w, h, hz = live.get(connector) or info["mode"]
        name = "Built-in" if internal else info["name"] or connector
        found.append(Screen(connector, internal, name, w, h, hz, "built-in" if internal else screen_kind(info)))
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
        elif kind == "Battery" and battery is None and read(dev + "/scope") != "Device":  # not a controller's
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


def user_home(user):
    """The Deck user's home. The service runs as root, so it can't just use $HOME."""
    try:
        return pwd.getpwnam(user).pw_dir
    except KeyError:
        return f"/home/{user}"


def steam_root(user):
    """The user's Steam directory: the native install, the ~/.steam symlink, or the flatpak."""
    home = user_home(user)
    for rel in (".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/data/Steam"):
        path = Path(sysfs(f"{home}/{rel}"))
        if (path / "steamapps").is_dir():
            return path
    return Path(sysfs(f"{home}/.local/share/Steam"))


def steam_libraries(steam):
    """Every Steam library (internal storage, SD card, ...) from libraryfolders.vdf, the main one first."""
    libraries = [steam]
    vdf = read(steam / "steamapps" / "libraryfolders.vdf", "")
    for p in re.findall(r'"path"\s+"([^"]+)"', vdf):
        lib = Path(sysfs(p.replace("\\\\", "\\")))
        if lib not in libraries:
            libraries.append(lib)
    return libraries


def find_app(steam, appid):
    """(library, installdir) of an installed Steam game, from its appmanifest; (None, None) if not installed."""
    for lib in steam_libraries(steam):
        acf = read(lib / "steamapps" / f"appmanifest_{appid}.acf")
        if acf is not None:
            m = re.search(r'"installdir"\s+"([^"]*)"', acf)
            return lib, m.group(1) if m else None
    return None, None


class GameNames:
    """Looks up game names from Steam's appmanifest files (internal storage and SD card)."""

    def __init__(self, user):
        self.steam = steam_root(user)
        self.cache = {}

    def get(self, appid):
        if appid is None:
            return None
        if appid not in self.cache:
            self.cache[appid] = self._lookup(appid)
        return self.cache[appid]

    def _lookup(self, appid):
        for lib in steam_libraries(self.steam):
            acf = read(lib / "steamapps" / f"appmanifest_{appid}.acf", "")
            m = re.search(r'"name"\s+"([^"]*)"', acf)
            if m:
                return m.group(1)
        return self._shortcut_name(int(appid))

    def _shortcut_name(self, appid):
        """Non-Steam games (emulators, EmuDeck entries...) live in the binary userdata/*/config/shortcuts.vdf."""
        for vdf in glob.glob(str(self.steam / "userdata" / "*" / "config" / "shortcuts.vdf")):
            try:
                data = Path(vdf).read_bytes()
            except OSError:
                continue
            for m in re.finditer(rb"\x02appid\x00(.{4})", data, re.S | re.I):
                if int.from_bytes(m.group(1), "little") != appid:
                    continue
                name = re.compile(rb"\x01appname\x00([^\x00]*)\x00", re.I).search(data, m.end())
                if name:
                    return name.group(1).decode(errors="replace")
        return None


KEY_A, KEY_Z, KEY_SPACE = 30, 44, 57
CONTROLLER_NAMES = re.compile(r"controller|gamepad|joystick|joy-con|x-?box|dualsense|dualshock|8bitdo|stadia", re.I)


def has_bit(words, n):
    """Bit n of a /proc/bus/input/devices bitmap (hex longs, most significant first)."""
    i = len(words) - 1 - n // 64
    return i >= 0 and (int(words[i], 16) >> (n % 64)) & 1 == 1


def input_devices():
    """Connected input devices as dicts: name, event node, kind ("deck", "controller", "kbm")."""
    devices = []
    for block in read(sysfs("/proc/bus/input/devices"), "").split("\n\n"):
        ids = re.search(r"Bus=(\w+) Vendor=(\w+) Product=(\w+)", block)
        name = re.search(r'N: Name="(.*)"', block)
        handlers = re.search(r"H: Handlers=(.*)", block)
        event = re.search(r"\bevent\d+", handlers.group(1)) if handlers else None
        if not (ids and name and event):
            continue
        bus, vendor, product = (x.lower() for x in ids.groups())
        name, handlers = name.group(1), handlers.group(1)
        keys = re.search(r"B: KEY=(.*)", block)
        keys = keys.group(1).split() if keys else []
        if "virtual" in name.lower() or (vendor, product) == ("28de", "11ff"):
            kind = None              # Steam Input's own virtual pads
        elif (vendor, product) == ("28de", "1205"):
            kind = "deck"            # the Deck's built-in controls
        elif bus not in ("0003", "0005"):
            kind = None              # only USB and Bluetooth; skips power button, touchscreen, etc.
        elif CONTROLLER_NAMES.search(name) or re.search(r"\bjs\d", handlers):
            kind = "controller"
        elif "mouse" in handlers or all(has_bit(keys, k) for k in (KEY_A, KEY_Z, KEY_SPACE)):
            kind = "kbm"
        else:
            kind = None              # headset buttons, media remotes...
        if kind:
            devices.append({"name": name, "event": event.group(0), "kind": kind, "usb_id": (vendor, product),
                            "id": f"{vendor}:{product}"})
    # Controller dongles often add keyboard/mouse interfaces (for macros). Those belong to the controller,
    # so they must never count as, or be opened as, a keyboard or mouse.
    controller_ids = {d["usb_id"] for d in devices if d["kind"] == "controller"}
    return [d for d in devices if not (d["kind"] == "kbm" and d["usb_id"] in controller_ids)]


def usb_device_dir(event):
    """The USB device directory behind /dev/input/eventN (the one with idVendor and power/control)."""
    try:
        path = Path(sysfs(f"/sys/class/input/{event}")).resolve()
    except OSError:
        return None
    for parent in [path, *path.parents]:
        if (parent / "idVendor").exists() and (parent / "power" / "control").exists():
            return parent
    return None


class InputWatcher:
    """Works out whether you're playing with the Deck's own controls, a controller, or keyboard and mouse.

    Only keyboards and mice are ever opened (by the root service) to see when they were last used.
    Controllers, including the Deck's built-in one, are never opened: Steam Input owns them, and reading
    them alongside Steam broke the left stick in at least one game. They count by being connected:
    keyboard/mouse if used in the last couple of minutes, else a connected controller, else the Deck."""

    RECENT = 120  # seconds an input counts as "in use" after it was last touched
    EVENT = struct.Struct("llHHi")
    EV_KEY, EV_REL, EV_ABS, ABS_HAT0X, ABS_HAT0Y = 1, 2, 3, 16, 17

    def __init__(self):
        self.devices, self.fds, self.last_used, self.scanned = [], {}, {}, 0.0

    def scan(self):
        self.devices = input_devices()
        wanted = {d["event"]: d for d in self.devices if d["kind"] == "kbm"}
        for event in list(self.fds):
            if event not in wanted:
                os.close(self.fds.pop(event)[0])
        for event, dev in wanted.items():
            if event not in self.fds:
                try:
                    self.fds[event] = (os.open(sysfs(f"/dev/input/{event}"), os.O_RDONLY | os.O_NONBLOCK), dev)
                except OSError:
                    pass  # not root: fall back to which devices are connected

    def poll(self):
        now = time.monotonic()
        if now - self.scanned > 10:
            self.scan()
            self.scanned = now
        for event, (fd, dev) in list(self.fds.items()):
            try:
                while data := os.read(fd, self.EVENT.size * 64):
                    for i in range(0, len(data) - self.EVENT.size + 1, self.EVENT.size):
                        _, _, etype, code, value = self.EVENT.unpack_from(data, i)
                        # Button/key presses, mouse movement, d-pad. Stick and trigger noise doesn't count.
                        if (etype == self.EV_KEY and value == 1) or etype == self.EV_REL or \
                                (etype == self.EV_ABS and code in (self.ABS_HAT0X, self.ABS_HAT0Y) and value):
                            self.last_used[dev["kind"]] = (now, dev["name"])
                            break
            except BlockingIOError:
                pass
            except OSError:  # unplugged
                os.close(fd)
                del self.fds[event]

    def current(self):
        """(kind, device name, vendor:product)."""
        self.poll()
        now = time.monotonic()
        recent = {k: v for k, v in self.last_used.items() if now - v[0] < self.RECENT}
        if recent:
            kind = max(recent, key=lambda k: recent[k][0])
            dev = next((d for d in self.devices if d["name"] == recent[kind][1]), {})
            return kind, recent[kind][1], dev.get("id", "")
        for kind in ("controller", "kbm"):
            dev = next((d for d in self.devices if d["kind"] == kind), None)
            if dev:
                return kind, dev["name"], dev["id"]
        return "deck", "Steam Deck controls", "28de:1205"


def controller_batteries():
    """Batteries of connected controllers: power supplies with scope "Device" (DualShock/DualSense, Xbox
    over Bluetooth, Switch controllers...). Returns dicts: name, percent (or None), level, charging."""
    found = []
    for dev in sorted(glob.glob(sysfs("/sys/class/power_supply/*"))):
        if read(dev + "/scope") != "Device" or read(dev + "/type") != "Battery":
            continue
        cap = read(dev + "/capacity", "")
        found.append({
            "name": read(dev + "/model_name") or Path(dev).name,
            "percent": int(cap) if cap.isdigit() else None,
            "level": read(dev + "/capacity_level", ""),
            "charging": read(dev + "/status") in ("Charging", "Full"),
        })
    return found


def battery_is_low(b):
    return not b["charging"] and ((b["percent"] is not None and b["percent"] <= 20)
                                  or b["level"] in ("Low", "Critical"))


def detect(cfg, names, was_low, inputs=None):
    settings = cfg.get("settings", {})
    ac, battery = power_state()
    threshold = settings.get("low_battery_percent", 20)
    # Hysteresis so we don't flap around the threshold.
    low = not ac and battery is not None and battery < (threshold + 5 if was_low else threshold)
    appid = running_appid()
    if inputs is None:
        inputs = InputWatcher()
    kind, device, device_id = inputs.current()
    return State(appid, names.get(appid), ac, battery, low, active_screen(screens()), kind, device, device_id)


# --- Profile resolution -----------------------------------------------------


def find_table(tables, *keys):
    if not tables:
        return None, None
    lowered = {str(k).lower(): (k, v) for k, v in tables.items()}
    for key in keys:
        if key is not None and str(key).lower() in lowered:
            return lowered[str(key).lower()]
    return None, None


def find_controller(tables, state):
    """[controller."..."] tables match the controller's name (any part of it, any case) or its vendor id,
    alone ("3537") or with the product ("3537:1022"). The id is handy when the name is generic, like a
    GameSir in Xbox mode showing up as "Generic X-Box pad"."""
    for key, table in (tables or {}).items():
        k = str(key).lower()
        if k in state.input_name.lower() or k in (state.input_id, state.input_id.split(":")[0]):
            return key, table
    return None, None


def layer_suffixes(state):
    """Names of the condition sub-tables that apply to this state, most general first ("" = the table itself)."""
    kind = None if state.screen.internal else state.screen.kind
    suffixes = ["", state.power, state.where, kind, state.input, f"{state.power}+{state.where}",
                kind and f"{state.power}+{kind}", f"{state.power}+{state.input}"]
    return [s for s in suffixes if s is not None]


def merge_layer(table, state, label, out, trail):
    """Merge one profile table and its condition sub-tables, most general first."""
    if not table:
        return
    for suffix in layer_suffixes(state):
        sub = table.get(suffix) if suffix else table
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
    if state.input == "controller":
        key, table = find_controller(cfg.get("controller"), state)
        merge_layer(table, state, f'controller."{key}"', out, trail)
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


def even_fps(hz, limit, min_ratio):
    """Highest frame cap at or under `limit` that the refresh rate divides evenly, so every frame is shown
    for the same number of refreshes (144Hz -> 72 or 48, 120Hz -> 60, 60Hz -> 60 or 30).
    Falls back to `limit` if the best even cap would be below limit * min_ratio (e.g. 75Hz -> 25)."""
    for n in range(1, 13):
        fps = hz / n
        if fps <= limit + 0.5 and abs(fps - round(fps)) < 0.1:
            return round(fps) if fps >= limit * min_ratio else limit
    return limit


def finalize(settings, state, panel):
    """Turn symbolic values ("refresh", "max", "auto", ...) into concrete numbers for this screen."""
    lo, hi = panel["refresh"]
    fps = settings["fps"]
    if state.screen.internal and fps == "auto":
        fps = min(int(settings["max_fps"]), hi)
    if state.screen.internal:
        # Game Mode sets the refresh rate itself: the highest rate the frame cap divides evenly, so
        # 40fps -> 80Hz and 60fps -> 60Hz on the OLED (no judder). The `refresh` setting is no longer used.
        hz = hi if fps in ("refresh", "half") else best_refresh(int(fps), lo, hi, highest=True)
    else:
        # External screens keep their own refresh rate, so fit the frame cap to it instead.
        hz = state.screen.refresh or 60
        if fps == "auto":
            fps = even_fps(hz, int(settings["max_fps"]), 0.75)
        elif fps not in ("refresh", "half") and int(fps) > 0:
            fps = even_fps(hz, int(fps), 0.85)
        hz = round(hz)
    if fps == "refresh":
        fps = hz
    elif fps == "half":
        fps = hz // 2
    gpu = settings["gpu_mhz"]
    return {
        "tdp": clamp(settings["tdp"], *TDP_RANGE),
        "auto_tdp": bool(settings["auto_tdp"]),
        "adaptive_fps": bool(settings["adaptive_fps"]),
        "gpu_mhz": "auto" if gpu == "auto" else clamp(gpu, *GPU_MHZ_RANGE),
        "fps": max(0, int(fps)),
        "refresh": hz if state.screen.internal else None,  # gamescope drives external refresh itself
        "smt": bool(settings["smt"]),
        "cpu_boost": bool(settings["cpu_boost"]),
        "cpu_epp": settings["cpu_epp"],
        "vrr": settings["vrr"],                # None = leave Steam's setting alone
        "allow_tearing": settings["allow_tearing"],
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
        self.originals = {}  # path -> value before DeckOpt first changed it, put back on stop
        self.charge_limit_on = False
        self.awake = set()  # controllers whose USB power saving is already handled
        self.atom_originals = {}  # Game Mode switches (VRR, tearing) -> (env, Steam's value)

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

    def write_restorable(self, path, value):
        if path not in self.originals:
            current = read(path)
            if current is None:
                return f"{Path(path).name}: not found"
            m = re.search(r"\[(\w+)\]", current)  # "always [madvise] never" style files
            self.originals[path] = m.group(1) if m else current
        return self.write(path, value)

    def hwmon(self, *names):
        for name_file in glob.glob(sysfs("/sys/class/hwmon/hwmon*/name")):
            if read(name_file) in names:
                return Path(name_file).parent
        return None

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

    def set_epp(self, pref):
        """CPU energy/performance bias (amd-pstate-epp, SteamOS 3.7.5+): "power" ... "performance"."""
        files = sorted(glob.glob(sysfs("/sys/devices/system/cpu/cpu[0-9]*/cpufreq/energy_performance_preference")))
        if not pref:
            return self.note("cpu_epp", None, "left as is")
        if not files:
            return self.note("cpu_epp", None, "not supported by this SteamOS version")
        available = read(Path(files[0]).with_name("energy_performance_available_preferences"), "").split()
        if available and pref not in available:
            return self.note("cpu_epp", False, f"\"{pref}\" is not one of: {', '.join(available)}")
        errors = [e for e in (self.write_restorable(f, pref) for f in files) if e]
        self.note("cpu_epp", not errors, errors[0] if errors else pref.replace("_", " "))

    def apply_memory_tweaks(self):
        errors = [e for e in (self.write_restorable(sysfs(p), v) for p, v in MEMORY_TWEAKS.items()) if e]
        self.memory_result = {"ok": not errors, "detail": errors[0] if errors else "CryoUtilities settings"}
        if errors:
            log.warning("memory tweaks: %s", errors[0])

    def fast_controller_polling(self):
        """Ask USB HID gamepads (wired PlayStation and generic pads) for input every 1ms instead of their
        default 4-8ms. Xbox-style pads (xpad driver, including GameSir in Xbox mode) already poll fast and
        aren't affected. Takes effect for controllers plugged in after this; restored when DeckOpt stops."""
        path = sysfs("/sys/module/usbhid/parameters/jspoll")
        if not os.path.exists(path):
            return {"ok": None, "detail": "not available on this system"}
        err = self.write_restorable(path, "1")
        return {"ok": not err, "detail": err or "1000Hz for wired USB pads (re-plug to apply)"}

    def keep_controllers_awake(self, devices):
        """Stops USB power saving on wireless controller dongles: a classic cause of a controller
        dropping and reconnecting over and over. The original setting is put back when DeckOpt stops."""
        for dev in devices:
            if dev["kind"] != "controller" or dev["event"] in self.awake:
                continue
            self.awake.add(dev["event"])
            usb = usb_device_dir(dev["event"])
            control = str(usb / "power" / "control") if usb else None
            if control and read(control) != "on":
                if not self.write_restorable(control, "on"):
                    log.info("keeping %s awake (USB power saving off)", dev["name"])

    def set_charge_limit(self, docked, limit):
        """Optional: cap charging while docked to spare the battery; put the user's own limit back after."""
        hwmon = self.hwmon("steamdeck_hwmon", "jupiter")
        path = str(hwmon / "max_battery_charge_level") if hwmon else None
        if not limit or not path or not os.path.exists(path):
            return
        if docked and not self.charge_limit_on:
            self.write_restorable(path, int(limit))
            self.charge_limit_on = True
            log.info("docked: charging capped at %s%%", limit)
        elif not docked and self.charge_limit_on:
            self.write(path, self.originals.pop(path))
            self.charge_limit_on = False
            log.info("undocked: charge limit back to normal")

    # Game Mode (gamescope). The frame cap goes through gamescope's control protocol, the same channel as
    # Steam's own framerate slider: `gamescopectl debug_set_fps_limit N` (gamescope 3.15+, SteamOS 3.6+).
    # Gamescope recomputes the limit from that every frame, so the old GAMESCOPE_FPS_LIMIT /
    # GAMESCOPE_DYNAMIC_REFRESH X properties get overwritten straight away; they're only a fallback for
    # older gamescope versions that don't have gamescopectl.

    def gamescope_env(self):
        """Environment for talking to the Game Mode session, or None when not in Game Mode."""
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
                    keys = ("DISPLAY", "XAUTHORITY", "GAMESCOPE_WAYLAND_DISPLAY", "XDG_RUNTIME_DIR")
                    steam_env = {k: env[k] for k in keys if k in env}
        if not in_game_mode:
            return None
        env = steam_env or {"DISPLAY": self.display}
        if "XDG_RUNTIME_DIR" not in env:
            try:
                env["XDG_RUNTIME_DIR"] = f"/run/user/{pwd.getpwnam(self.user).pw_uid}"
            except KeyError:
                pass
        return env

    def run_as_user(self, env, *cmd):
        """Runs a command in the user's Game Mode session. Returns (error or None, stdout)."""
        if not shutil.which(cmd[0]):
            return f"{cmd[0]} is not installed", ""
        full = ["env"] + [f"{k}={v}" for k, v in env.items()] + list(cmd)
        if os.geteuid() == 0:
            full = ["runuser", "-u", self.user, "--"] + full
        try:
            res = subprocess.run(full, capture_output=True, text=True, timeout=5)
        except (OSError, subprocess.TimeoutExpired) as e:
            return str(e), ""
        if res.returncode:
            return res.stderr.strip() or f"{cmd[0]} exited with {res.returncode}", ""
        return None, res.stdout

    def send_fps(self, env, fps):
        """Sets the frame cap (0 = off). Gamescope also switches the built-in screen to the highest refresh
        rate that's a multiple of it, exactly like Steam's slider. Returns (error or None, method)."""
        if self.dry_run:
            log.info("[dry-run] gamescopectl debug_set_fps_limit %s", fps)
            return None, "gamescopectl"
        err, _ = self.run_as_user(env, "gamescopectl", "debug_set_fps_limit", str(fps))
        if not err:
            return None, "gamescopectl"
        self.warn_once("gamescopectl", "gamescopectl failed (%s), falling back to X properties", err)
        err2, _ = self.run_as_user(env, "xprop", "-root", "-f", "GAMESCOPE_FPS_LIMIT", "32c",
                                   "-set", "GAMESCOPE_FPS_LIMIT", str(fps))
        return (None, "xprop") if not err2 else (f"{err}; {err2}", None)

    def set_display_flag(self, env, key, atom, value, on_label):
        """Game Mode switches that stay put once set (VRR, tearing), unlike the frame cap. Steam's own
        value is remembered and put back when DeckOpt stops."""
        if value is None:
            return
        if self.dry_run:
            return self.note(key, True, on_label if value else "off")
        if atom not in self.atom_originals:
            _, out = self.run_as_user(env, "xprop", "-root", atom)
            m = re.search(r"=\s*(\d+)", out)
            self.atom_originals[atom] = (env, int(m.group(1)) if m else 0)
        err, _ = self.run_as_user(env, "xprop", "-root", "-f", atom, "32c", "-set", atom, str(int(bool(value))))
        self.note(key, not err, err or (on_label if value else "off"))

    def apply(self, final):
        self.results = {}
        self.set_tdp(final["tdp"])
        self.set_gpu(final["gpu_mhz"])
        self.set_cpu(final["smt"], final["cpu_boost"])
        self.set_epp(final.get("cpu_epp"))
        # FPS cap and refresh live in gamescope, so they only apply in Game Mode.
        env = self.gamescope_env()
        if env is None:
            self.note("fps", None, "only works in Game Mode")
            self.note("refresh", None, "only works in Game Mode")
            return
        self.set_display_flag(env, "vrr", "GAMESCOPE_VRR_ENABLED", final.get("vrr"), "on (if the screen supports it)")
        self.set_display_flag(env, "allow_tearing", "GAMESCOPE_ALLOW_TEARING", final.get("allow_tearing"),
                              "allowed (lowest lag)")
        err, method = self.send_fps(env, final["fps"])
        self.note("fps", not err, err or final["fps"] or "unlimited")
        if err:
            self.note("refresh", False, "not changed, the FPS cap couldn't be set")
        elif not final["refresh"]:
            self.note("refresh", None, "external screen sets its own")
        elif method == "gamescopectl":
            self.note("refresh", True, f"{final['refresh']}Hz (Game Mode matches it to the FPS cap)")
        else:  # old gamescope: the X property still works there
            err, _ = self.run_as_user(env, "xprop", "-root", "-f", "GAMESCOPE_DYNAMIC_REFRESH", "32c",
                                      "-set", "GAMESCOPE_DYNAMIC_REFRESH", str(final["refresh"]))
            self.note("refresh", not err, err or f"{final['refresh']}Hz")

    def verify(self, final):
        """Re-asserts the frame cap and lists settings something else (usually Steam) has changed.

        Gamescope has no way to read the active frame cap back, and Steam resends its own (often "off")
        when a game starts or its Performance settings are touched, so the cap is simply sent again."""
        problems = []
        hwmon = self.amdgpu_hwmon()
        cap = read(hwmon / "power1_cap") if hwmon else None
        if cap and cap.isdigit() and round(int(cap) / 1_000_000) != final["tdp"]:
            problems.append(f"Power limit is {round(int(cap) / 1_000_000)}W instead of {final['tdp']}W "
                            "(Steam's TDP limit is probably on)")
        env = self.gamescope_env()
        if env and self.results.get("fps", {}).get("ok"):
            err, _ = self.send_fps(env, final["fps"])
            if err:
                problems.append(f"Couldn't re-send the FPS cap: {err}")
            for key, atom in (("vrr", "GAMESCOPE_VRR_ENABLED"), ("allow_tearing", "GAMESCOPE_ALLOW_TEARING")):
                if final.get(key) is not None and self.results.get(key, {}).get("ok"):
                    self.run_as_user(env, "xprop", "-root", "-f", atom, "32c", "-set", atom, str(int(final[key])))
        return problems

    def restore(self):
        log.info("restoring stock settings")
        self.apply({**DEFAULTS, "fps": 0, "refresh": None})
        for path, value in self.originals.items():  # memory tweaks, CPU energy preference, charge limit
            self.write(path, value)
        for atom, (env, value) in self.atom_originals.items():  # VRR, tearing
            self.run_as_user(env, "xprop", "-root", "-f", atom, "32c", "-set", atom, str(value))


def cpu_times():
    times = {}
    for line in read(sysfs("/proc/stat"), "").splitlines():
        if re.match(r"cpu\d+ ", line):
            f = [int(x) for x in line.split()[1:]]
            times[line.split()[0]] = (sum(f), f[3] + (f[4] if len(f) > 4 else 0))  # total, idle + iowait
    return times


class AutoTDP:
    """Battery saver that works for every game, with or without a profile.

    Watches how busy the GPU and the busiest CPU core are. Plenty of idle time means the game is
    easily hitting its frame cap, so the power limit steps down; the GPU or a core maxing out means it's
    struggling, so it steps back up. It never goes above the profile's tdp, and what it settles on is
    remembered per game, power source and screen, so the next session starts there."""

    WARMUP, STEP_EVERY = 20, 10  # seconds

    def __init__(self, tuner, settings, path):
        self.tuner, self.path = tuner, path
        self.floor = int(settings.get("auto_tdp_min", 5))
        try:
            self.learned = json.loads(path.read_text())
        except (OSError, ValueError):
            self.learned = {}
        self.begin(None, None)

    def begin(self, key, ceiling):
        self.key, self.ceiling = key, ceiling
        if key:
            self.current = clamp(self.learned.get(key, ceiling), min(self.floor, ceiling), ceiling)
        self.samples, self.prev_cpu = [], None
        self.started = self.last_step = time.monotonic()

    def sample(self):
        """Call every poll. Returns a new power limit when it's time to change it, else None."""
        if not self.key:
            return None
        hwmon = self.tuner.amdgpu_hwmon()
        busy = read(self.tuner.gpu_device(hwmon) / "gpu_busy_percent", "") if hwmon else ""
        cpu = cpu_times()
        if busy.isdigit() and self.prev_cpu:
            cores = [100 * (1 - (cpu[c][1] - self.prev_cpu[c][1]) / max(1, cpu[c][0] - self.prev_cpu[c][0]))
                     for c in cpu if c in self.prev_cpu]
            self.samples.append((int(busy), max(cores, default=0)))
        self.prev_cpu = cpu
        now = time.monotonic()
        if now - self.started < self.WARMUP or now - self.last_step < self.STEP_EVERY or not self.samples:
            return None
        gpu = sum(g for g, _ in self.samples) / len(self.samples)
        core = sum(c for _, c in self.samples) / len(self.samples)
        self.samples, self.last_step = [], now
        new = self.current
        if gpu > 92 or core > 92:
            new = min(self.ceiling, self.current + 2)  # struggling: recover quickly
        elif gpu < 70 and core < 80:
            new = max(self.floor, self.current - 1)    # headroom: ease down
        log.debug("auto tdp: gpu %d%%, busiest core %d%%, %dW -> %dW", gpu, core, self.current, new)
        if new == self.current:
            return None
        self.current = self.learned[self.key] = new
        self.save()
        return new

    def save(self):
        try:
            self.path.write_text(json.dumps(self.learned, indent=1))
        except OSError as e:
            log.debug("could not save learned settings: %s", e)


def pack_string(text):
    raw = text.encode() + b"\0"
    return struct.pack("<I", len(raw)) + raw + b"\0" * (-len(raw) % 4)


def unpack_string(payload, offset):
    length, = struct.unpack_from("<I", payload, offset)
    text = payload[offset + 4:offset + 4 + length - 1].decode(errors="replace")
    return text, offset + 4 + length + (-length % 4)


class GamescopePerf:
    """Reads a game's real frame times from gamescope (3.16.20+), the data Steam's overlay is built on.

    A minimal Wayland client written against the wire protocol directly, so nothing extra needs installing:
    get the registry, bind gamescope_control (version 6+), then send request_app_performance_stats(appid);
    gamescope answers with app_performance_stats on the game's next frame."""

    STATS_REQUEST, STATS_EVENT = 6, 3  # opcodes in gamescope-control.xml

    def __init__(self, socket_path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(1.0)
        self.sock.connect(socket_path)
        self.buf = b""
        self.send(1, 1, struct.pack("<I", 2))  # wl_display.get_registry -> registry is object 2
        self.send(1, 0, struct.pack("<I", 3))  # wl_display.sync -> callback 3 fires once globals are listed
        control = None
        while True:
            obj, opcode, payload = self.receive()
            if obj == 2 and opcode == 0:  # wl_registry.global(name, interface, version)
                name, = struct.unpack_from("<I", payload)
                interface, offset = unpack_string(payload, 4)
                version, = struct.unpack_from("<I", payload, offset)
                if interface == "gamescope_control":
                    control = (name, version)
            elif obj == 3:
                break
        if not control or control[1] < 6:
            self.close()
            raise OSError("this gamescope is too old to report frame times (needs 3.16.20+)")
        # wl_registry.bind(name, interface, version, new id 4)
        self.send(2, 0, struct.pack("<I", control[0]) + pack_string("gamescope_control") + struct.pack("<II", 6, 4))

    def send(self, obj, opcode, payload=b""):
        self.sock.sendall(struct.pack("<II", obj, (8 + len(payload)) << 16 | opcode) + payload)

    def receive(self):
        while True:
            if len(self.buf) >= 8:
                obj, size_op = struct.unpack_from("<II", self.buf)
                size = size_op >> 16
                if len(self.buf) >= size:
                    payload, self.buf = self.buf[8:size], self.buf[size:]
                    if obj == 1 and size_op & 0xFFFF == 0:  # wl_display.error
                        raise OSError(f"gamescope protocol error: {unpack_string(payload, 8)[0]}")
                    return obj, size_op & 0xFFFF, payload
            chunk = self.sock.recv(4096)
            if not chunk:
                raise OSError("gamescope closed the connection")
            self.buf += chunk

    def fps(self, appid, budget=0.5):
        """Average frame rate over a short burst of frame-time samples, or None if the game isn't drawing."""
        times, deadline = [], time.monotonic() + budget
        while time.monotonic() < deadline and len(times) < 20:
            self.send(4, self.STATS_REQUEST, struct.pack("<I", int(appid) & 0xFFFFFFFF))
            self.sock.settimeout(max(0.05, deadline - time.monotonic()))
            try:
                while True:
                    obj, opcode, payload = self.receive()
                    if obj == 4 and opcode == self.STATS_EVENT:
                        _, lo, hi = struct.unpack_from("<III", payload)
                        times.append(hi << 32 | lo)
                        break
            except socket.timeout:
                break
        times = sorted(t for t in times if t > 0)
        return 1e9 / times[len(times) // 2] if times else None

    def close(self):
        self.sock.close()


def fps_ladder(top, state, panel, floor=30):
    """Evenly paced caps from `top` down: each has a refresh rate it divides exactly.
    OLED: 60 -> 45 -> 40 -> 30; LCD: 60 -> 45 -> 40 -> 30; 60Hz TV: 60 -> 30; 120Hz: 60 -> 40 -> 30."""
    if state.screen.internal:
        lo, hi = panel["refresh"]
        steps = [top] + [f for f in (60, 45, 40, 30) if f < top]
        steps = [f for f in steps if any(hz % f == 0 for hz in range(lo, hi + 1))]
    else:
        hz = state.screen.refresh or 60
        steps = [top] + [round(hz / n) for n in range(1, 13)
                         if abs(hz / n - round(hz / n)) < 0.1 and floor - 5 <= round(hz / n) < top]
    return sorted(set(steps), reverse=True)


class AdaptiveFPS:
    """While plugged in: aim for the profile's cap (60 by default). If the game can't hold it, step down to
    the next evenly paced cap; once it sits comfortably at a lower cap with GPU to spare, try one step up,
    and back off for a while if that doesn't hold. Uses real frame times from gamescope. The level each
    game settles at is remembered per power source and screen."""

    WARMUP, WINDOW, UP_AFTER, COOLDOWN = 30, 10, 60, 300  # seconds

    def __init__(self, tuner, learned, save):
        self.tuner, self.learned, self.save = tuner, learned, save
        self.perf, self.perf_error, self.perf_retry = None, None, 0.0
        self.begin(None, None, None)

    def begin(self, key, appid, ladder):
        self.key, self.appid, self.ladder = key, appid, ladder
        if key:
            remembered = self.learned.get(key)
            self.cap = remembered if remembered in ladder else ladder[0]
        now = time.monotonic()
        self.readings, self.gpu = [], []
        self.started = self.last_change = self.last_decision = now
        self.low_windows, self.trial_from, self.cooldown_until, self.failures = 0, None, {}, {}
        self.measured = None

    def connect(self, env):
        if self.perf or time.monotonic() < self.perf_retry:
            return self.perf
        path = Path(env.get("XDG_RUNTIME_DIR", "/run/user/1000"), env.get("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0"))
        try:
            self.perf, self.perf_error = GamescopePerf(sysfs(str(path))), None
        except OSError as e:
            self.perf, self.perf_error, self.perf_retry = None, str(e), time.monotonic() + 60
            log.warning("adaptive FPS can't read frame times: %s", e)
        return self.perf

    def sample(self, env):
        """Call every poll. Returns a new cap when it should change, else None."""
        if not self.key or not env or not self.connect(env):
            return None
        try:
            fps = self.perf.fps(self.appid)
        except OSError as e:
            log.debug("frame time read failed: %s", e)
            self.perf.close()
            self.perf = None
            return None
        now = time.monotonic()
        if fps:
            self.readings.append((now, fps))
        hwmon = self.tuner.amdgpu_hwmon()
        busy = read(self.tuner.gpu_device(hwmon) / "gpu_busy_percent", "") if hwmon else ""
        if busy.isdigit():
            self.gpu.append((now, int(busy)))
        if now - self.started < self.WARMUP or now - self.last_decision < self.WINDOW:
            return None
        self.last_decision = now
        recent = sorted(f for t, f in self.readings if now - t <= self.WINDOW)
        gpu = [g for t, g in self.gpu if now - t <= self.WINDOW]
        self.readings = [(t, f) for t, f in self.readings if now - t <= self.WINDOW]
        self.gpu = [(t, g) for t, g in self.gpu if now - t <= self.WINDOW]
        if len(recent) < 3:
            return None  # paused, loading, or not drawing
        self.measured = recent[len(recent) // 2]
        i = self.ladder.index(self.cap)
        if self.measured < self.cap * 0.9:
            self.low_windows += 1
            if self.low_windows >= 2 and i + 1 < len(self.ladder):
                if self.trial_from is not None and now - self.last_change < 2 * self.UP_AFTER:
                    # The step up didn't hold. Wait before trying again, twice as long after each failure,
                    # so a game that really can't hold it isn't put through a rough patch every few minutes.
                    fails = self.failures[self.cap] = self.failures.get(self.cap, 0) + 1
                    self.cooldown_until[self.cap] = now + self.COOLDOWN * 2 ** (fails - 1)
                self.trial_from = None
                return self.change(self.ladder[i + 1], f"averaging {self.measured:.0f}")
            return None
        self.low_windows = 0
        headroom = gpu and sum(gpu) / len(gpu) < 80
        if (i > 0 and now - self.last_change >= self.UP_AFTER and self.measured >= self.cap * 0.97 and headroom
                and self.cooldown_until.get(self.ladder[i - 1], 0) < now):
            self.trial_from = self.cap
            return self.change(self.ladder[i - 1], "steady with GPU to spare, trying higher")
        return None

    def change(self, cap, why):
        log.info("adaptive FPS: %d -> %d (%s)", self.cap, cap, why)
        self.cap, self.last_change, self.low_windows, self.measured = cap, time.monotonic(), 0, None
        self.learned[self.key] = cap
        self.save()
        return cap


# --- In-game graphics settings ---------------------------------------------
#
# Some games only run well on the Deck once their own graphics settings are turned down. A [graphics."<appid>"]
# rule names the game's settings file and the values to put in it, with .battery/.ac/.tv/... sub-tables like
# the system profiles. Games read that file when they start and write it back when they quit, so it is only
# ever edited while the game is NOT running: right after it quits, when the charger or screen changes with no
# game open, and when the service starts. What goes in is for the *next* launch, chosen for the state at the
# time of writing. Only keys the game itself has written are changed; nothing is ever added.


def write_atomic(path, data):
    """Replaces a file's contents in one step (temp file beside it, then rename) and keeps the owner, group and
    permissions: the service runs as root, but these are the user's files."""
    st = path.stat()
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".deckopt")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, stat.S_IMODE(st.st_mode))
        try:
            os.chown(tmp, st.st_uid, st.st_gid)
        except PermissionError:
            pass  # not root: it's our own file anyway
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


class IniFile:
    """Changes values in an ini file and nothing else: encoding (UTF-8 or UTF-16, with or without a BOM), line
    endings, key order, spacing, comments and sections all come back byte for byte. Only keys that already
    exist are touched; which keys exist is the game's business."""

    BOMS = ((codecs.BOM_UTF8, "utf-8"), (codecs.BOM_UTF16_LE, "utf-16-le"), (codecs.BOM_UTF16_BE, "utf-16-be"))
    SECTION = re.compile(r"^\s*\[(.*?)\]\s*$")
    KEY = re.compile(r"^(\s*)([^\s=;#\[][^=]*?)(\s*=\s*)(.*?)(\s*)$")

    def __init__(self, data):
        self.bom, self.encoding, self.changed = b"", "utf-8", False
        for bom, encoding in self.BOMS:
            if data.startswith(bom):
                self.bom, self.encoding = bom, encoding
                break
        else:
            if data.count(b"\0") > len(data) // 4:  # UTF-16 without a BOM
                self.encoding = "utf-16-le" if data[1:2] == b"\0" else "utf-16-be"
        body = data[len(self.bom):]
        try:
            text = body.decode(self.encoding)
        except UnicodeDecodeError:
            self.encoding = "latin-1"  # unknown 8-bit encoding: keep every byte as it is
            text = body.decode(self.encoding)
        self.lines = []  # (content, line ending)
        for raw in re.findall(r"[^\r\n]*(?:\r\n|\r|\n)|[^\r\n]+$", text):
            content = raw.rstrip("\r\n")
            self.lines.append((content, raw[len(content):]))

    def set(self, section, key, value):
        """Returns True if the key is in the file (now set to value), False if it isn't (nothing is added)."""
        current = ""
        for i, (content, ending) in enumerate(self.lines):
            m = self.SECTION.match(content)
            if m:
                current = m.group(1).strip().lower()
                continue
            if current != section.lower():
                continue
            m = self.KEY.match(content)
            if m and m.group(2).lower() == key.lower():
                if m.group(4) != value:
                    self.lines[i] = (m.group(1) + m.group(2) + m.group(3) + value + m.group(5), ending)
                    self.changed = True
                return True
        return False

    def dump(self):
        return self.bom + "".join(content + ending for content, ending in self.lines).encode(self.encoding)


GRAPHICS_FORMATS = {"ini": IniFile}  # json/xml editors would slot in here with the same set()/dump()/changed
BACKUP_DIR = Path(os.environ.get("DECKOPT_BACKUPS") or Path(__file__).with_name("graphics-backup"))


def ini_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float):
        return f"{value:g}"
    return str(value)


def graphics_values(rule, state):
    """{section: {key: value}} for this state, layered like the system profiles: the rule's own `values`, then
    those of .battery/.ac, .internal/.external, .tv/.monitor, .deck/.controller/.kbm, ."ac+tv" and so on."""
    out, trail = {}, []
    for suffix in layer_suffixes(state):
        sub = rule.get(suffix) if suffix else rule
        values = sub.get("values") if isinstance(sub, dict) else None
        if isinstance(values, dict):
            for section, keys in values.items():
                if isinstance(keys, dict):
                    out.setdefault(section, {}).update(keys)
            trail.append(suffix or "base")
    return out, trail


class Graphics:
    """Applies the [graphics."<appid>"] rules and records, per game, what happened (for DeckOpt Check)."""

    def __init__(self, cfg, dry_run=False):
        self.user = cfg.get("settings", {}).get("user", "deck")
        self.dry_run = dry_run
        self.steam = steam_root(self.user)
        self.results = {}  # appid -> {"game", "ok", "detail", "time"}; ok: True applied, None waiting, False failed
        self.warned = set()

    @staticmethod
    def rules(cfg):
        """Game rules (keyed by AppId) with their preset merged in. Non-numeric keys are presets, e.g. `unreal`."""
        tables = cfg.get("graphics") or {}
        rules = {}
        for key, rule in tables.items():
            if not isinstance(rule, dict) or not str(key).isdigit():
                continue
            preset = tables.get(rule.get("preset"))
            rules[str(key)] = deep_merge(preset, rule) if isinstance(preset, dict) else rule
        return rules

    def apply_all(self, cfg, state, names):
        if running_appid() is not None:
            return  # never while a game is running: it would overwrite the file on exit anyway
        for appid, rule in self.rules(cfg).items():
            self.apply(appid, rule, state, names.get(appid) or f"game {appid}")

    def locate(self, appid, rule):
        """The game's settings file, or (None, why not)."""
        lib, installdir = find_app(self.steam, appid)
        if lib is None:
            return None, (None, "not installed")
        if rule.get("root", "prefix") == "install":
            base = lib / "steamapps" / "common" / (installdir or "")
        else:  # the Proton prefix's Windows user folder: Documents, AppData, ...
            base = lib / "steamapps" / "compatdata" / appid / "pfx" / "drive_c" / "users" / "steamuser"
        patterns = rule.get("file")
        patterns = [patterns] if isinstance(patterns, str) else list(patterns or [])
        found = []
        for pattern in patterns:
            for path in sorted(glob.glob(str(base / pattern))):
                if os.path.isfile(path) and not any(os.path.samefile(path, f) for f in found):
                    found.append(path)
        if not found:
            return None, (None, "waiting for the first launch (the game writes its settings file then)")
        if len(found) > 1:
            return None, (False, f"{len(found)} settings files match, not sure which to edit")
        return Path(found[0]), None

    def apply(self, appid, rule, state, game):
        result = self.results[appid] = {"game": game, "ok": None, "detail": "", "time": time.time()}
        path, problem = self.locate(appid, rule)
        if problem:
            result["ok"], result["detail"] = problem
            return
        editor = GRAPHICS_FORMATS.get(rule.get("format", "ini"))
        if not editor:
            result["ok"], result["detail"] = False, f"unknown format \"{rule.get('format')}\""
            return
        values, trail = graphics_values(rule, state)
        wanted = [(s, k, v) for s, keys in values.items() for k, v in keys.items()]
        if not wanted:
            result["detail"] = "nothing to set for this state"
            return
        try:
            data = path.read_bytes()
            self.backup(appid, path, data)
            doc = editor(data)
            skipped = [f"{s}.{k}" if s else k for s, k, v in wanted if not doc.set(s, k, ini_value(v))]
            if doc.changed and not self.dry_run:
                write_atomic(path, doc.dump())
        except OSError as e:
            result["ok"], result["detail"] = False, f"{path.name}: {e.strerror or e}"
            log.warning("game settings for %s not applied: %s", game, result["detail"])
            return
        where = "built-in screen" if state.screen.internal else state.screen.kind
        label = f"{'charger' if state.ac else 'battery'} + {where}"
        if len(values) == 1:  # one section: the key names alone are clearer
            skipped = [k.split(".", 1)[1] if "." in k else k for k in skipped]
        short = ", ".join(skipped[:3]) + (f", +{len(skipped) - 3} more" if len(skipped) > 3 else "")
        if len(skipped) == len(wanted):
            result["ok"] = False
            result["detail"] = f"none of the keys are in {path.name} ({short})"
        else:
            result["ok"] = True
            result["detail"] = f"{len(wanted) - len(skipped)} settings for {label}"
            if skipped:
                result["detail"] += f"; {len(skipped)} not in the file, skipped ({short})"
        if doc.changed:
            log.info("game settings for %s: %s (%s)", game, result["detail"], " > ".join(trail))
        if skipped and (appid, tuple(skipped)) not in self.warned:
            self.warned.add((appid, tuple(skipped)))
            log.warning("game settings for %s: keys not in %s, skipped: %s", game, path.name, ", ".join(skipped))

    def backup(self, appid, path, data):
        """Keeps the untouched original once (first time DeckOpt sees the file), for restore_all()."""
        dest = BACKUP_DIR / appid / path.name
        if dest.exists() or self.dry_run:
            return
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        dest.with_name(dest.name + ".path").write_text(str(path))

    @staticmethod
    def restore_all():
        """Puts every backed-up settings file back as it was and forgets the backup. Used when DeckOpt is turned
        off (`deckopt toggle`) and on uninstall, not on every service stop: a restart would otherwise undo the
        settings for nothing, and shutdown is no time to be writing into the user's files.
        Returns [(game dir, path, error or None)]."""
        done = []
        for side in sorted(BACKUP_DIR.glob("*/*.path")):
            original = side.with_name(side.name[:-len(".path")])
            target = Path(side.read_text().strip())
            if running_appid() == side.parent.name:
                done.append((side.parent.name, target, "the game is running, try again after quitting"))
                continue
            try:
                if target.exists():
                    write_atomic(target, original.read_bytes())
                original.unlink()
                side.unlink()
                done.append((side.parent.name, target, None))
            except OSError as e:
                done.append((side.parent.name, target, str(e)))
            try:
                side.parent.rmdir()
            except OSError:
                pass
        return done


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


INPUT_LABELS = {"deck": "Deck controls", "controller": "controller", "kbm": "keyboard+mouse"}


def input_label(state):
    return INPUT_LABELS[state.input] + (f" ({state.input_name})" if state.input != "deck" else "")


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
    if final.get("cpu_epp"):
        knobs += f", cpu {final['cpu_epp'].replace('_', ' ')}"
    return (f"game: {game} | power: {power} | screen: {state.screen.describe()} | "
            f"input: {input_label(state)}\n"
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


LEARNED_FILE = STATE_FILE.with_name("learned.json")


def set_cap(final, state, panel, adaptive):
    final["fps"] = adaptive.cap
    if state.screen.internal:
        lo, hi = panel["refresh"]
        final["refresh"] = best_refresh(adaptive.cap, lo, hi, highest=True)


def note_adaptive(tuner, final, adaptive):
    r = tuner.results.get("fps")
    if not adaptive.key or not r or not r["ok"]:
        return
    if adaptive.perf_error:
        r["detail"] = f"{final['fps']} (adaptive is off: {adaptive.perf_error})"
        return
    measured = f", game running at {adaptive.measured:.0f}" if adaptive.measured else ""
    r["detail"] = f"{final['fps']} (adaptive, aiming for {adaptive.ladder[0]}{measured})"


def note_auto(tuner, final):
    if "tdp_max" in final and tuner.results.get("tdp", {}).get("ok"):
        tuner.results["tdp"]["detail"] = f"{final['tdp']}W (auto-tuned, up to {final['tdp_max']}W)"


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
        "screen": state.screen.describe(), "input": input_label(state), "target": final, "results": dict(tuner.results),
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
    graphics = Graphics(cfg, args.dry_run)
    auto = AutoTDP(tuner, cfg.get("settings", {}), LEARNED_FILE)
    adaptive = AdaptiveFPS(tuner, auto.learned, auto.save)
    inputs = InputWatcher()
    panel = panel_info()
    log.info("DeckOpt started (%s panel, config %s)", panel["model"], path)
    status = {"version": read(Path(__file__).with_name("VERSION"), "dev"), "started": time.time(),
              "current": None, "last_game": None, "events": [], "memory": None, "graphics": {}}
    # CLOCK_MONOTONIC stops while asleep and CLOCK_BOOTTIME doesn't, so a jump in the gap means we just woke up.
    boottime = getattr(time, "CLOCK_BOOTTIME", None)  # Linux only
    sleep_gap = 0.0

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    last, low, applied_at, settled, verified_at = None, False, 0.0, True, 0.0
    prev_appid, graphics_key, graphics_due = None, None, 0.0  # due straight away at start (if no game runs)
    state = final = None
    try:  # everything changed from here on is put back by tuner.restore(), even after a crash
        if cfg.get("settings", {}).get("memory_tweaks", False):
            tuner.apply_memory_tweaks()
            status["memory"] = tuner.memory_result
        if cfg.get("settings", {}).get("fast_controller_polling", False):
            status["polling"] = tuner.fast_controller_polling()
        warned_batteries = set()
        if boottime is not None:
            sleep_gap = time.clock_gettime(boottime) - time.monotonic()
        while True:
            settings = cfg.get("settings", {})
            if path.exists() and path.stat().st_mtime != mtime:
                try:
                    cfg, path = load_config(path)
                    mtime = path.stat().st_mtime
                    last = graphics_key = None  # force a re-apply with the new profiles
                    log.info("config reloaded")
                except tomllib.TOMLDecodeError as e:
                    mtime = path.stat().st_mtime
                    log.error("config has an error, keeping the old one: %s", e)

            gap = time.clock_gettime(boottime) - time.monotonic() if boottime is not None else 0.0
            if gap - sleep_gap > 5:
                log.info("woke from sleep, re-applying settings")
                last = None  # some settings reset during suspend
                auto.prev_cpu = None
            sleep_gap = gap

            state = detect(cfg, names, low, inputs)
            if settings.get("keep_controllers_awake", True):
                tuner.keep_controllers_awake(inputs.devices)
            batteries = controller_batteries()
            for b in batteries:
                if battery_is_low(b) and b["name"] not in warned_batteries:
                    warned_batteries.add(b["name"])
                    log.warning("controller battery low: %s %s", b["name"], f"{b['percent']}%" if b["percent"] is not None else b["level"])
                elif not battery_is_low(b):
                    warned_batteries.discard(b["name"])
            if batteries != status.get("controller_batteries"):
                status["controller_batteries"] = batteries
                if status["current"]:
                    save_state(status)
            low = state.low_battery
            key = (state.appid, state.ac, state.low_battery, state.screen, state.input)
            now = time.monotonic()
            tuner.set_charge_limit(state.ac and not state.screen.internal, settings.get("docked_charge_limit", 0))
            # The games' own settings files: written only while no game runs, for the next launch. A game that
            # has just quit has rewritten its file, so ours go in again a moment later.
            if state.appid is None and prev_appid is not None:
                graphics_due = now + 3
            elif state.appid is None and graphics_due is None and key[1:] != graphics_key:
                graphics_due = now
            prev_appid = state.appid
            if graphics_due is not None and state.appid is None and now >= graphics_due:
                graphics.apply_all(cfg, state, names)
                status["graphics"] = graphics.results
                save_state(status)
                graphics_key, graphics_due = key[1:], None
            # Steam applies its own per-game settings a few seconds after launch, so apply once more after that.
            resettle = not settled and now - applied_at >= settings.get("settle_seconds", 8)
            if key != last or resettle:
                values, trail = resolve(cfg, state)
                final = finalize(values, state, panel)
                changed = key != last
                if final["auto_tdp"] and state.appid:
                    final["tdp_max"] = final["tdp"]
                    if changed or auto.key is None:
                        auto.begin(f"{state.appid}|{state.power}|{state.where}", final["tdp"])
                    final["tdp"] = auto.current
                else:
                    auto.begin(None, None)
                env = tuner.gamescope_env()
                if final["adaptive_fps"] and state.appid and env and final["fps"] > 0:
                    if changed or adaptive.key is None:
                        key_name = f"fps|{state.appid}|{state.power}|{state.where}"
                        adaptive.begin(key_name, state.appid, fps_ladder(final["fps"], state, panel))
                    set_cap(final, state, panel, adaptive)
                else:
                    adaptive.begin(None, None, None)
                if changed:
                    log.info(summarize(state, final, trail))
                    knobs = f"{final['tdp']}W, {final['fps'] or 'unlimited'} FPS"
                    status["events"] = (status["events"] + [
                        f"{time.strftime('%H:%M')}  {game_label(state) or 'no game'} | {power_label(state)} | "
                        f"{state.screen.name} | {INPUT_LABELS[state.input]} -> {knobs}"])[-8:]
                tuner.apply(final)
                note_auto(tuner, final)
                note_adaptive(tuner, final, adaptive)
                for key_name, r in tuner.results.items():
                    if r["ok"] is False:
                        log.warning("%s not applied: %s", key_name, r["detail"])
                settled, applied_at, last = not changed, now, key
                verified_at = 0.0 if resettle else now  # check right after the settle pass
                record(status, state, final, tuner, [])
            new_cap = adaptive.sample(tuner.gamescope_env())
            if new_cap is not None:
                set_cap(final, state, panel, adaptive)
                err, _ = tuner.send_fps(tuner.gamescope_env() or {}, final["fps"])
                tuner.note("fps", not err, err or final["fps"])
                if final["refresh"]:
                    tuner.note("refresh", True, f"{final['refresh']}Hz (Game Mode matches it to the FPS cap)")
                note_adaptive(tuner, final, adaptive)
                record(status, state, final, tuner, status["current"]["problems"])
            elif adaptive.key and adaptive.measured and tuner.results.get("fps", {}).get("ok"):
                note_adaptive(tuner, final, adaptive)  # keep the measured frame rate in the check fresh
            new_tdp = auto.sample()
            if new_tdp is not None:
                log.info("auto power limit for %s: %dW -> %dW", game_label(state), final["tdp"], new_tdp)
                final["tdp"] = new_tdp
                tuner.set_tdp(new_tdp)
                note_auto(tuner, final)
                record(status, state, final, tuner, status["current"]["problems"])
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
    line(None, f"Right now: {game_label(state) or 'no game running'}, {power_label(state)}, {state.screen.describe()}, "
               f"{INPUT_LABELS[state.input]}")

    if not data or not data.get("current"):
        line(False, "The service hasn't recorded anything yet. Fix: deckopt restart, wait 20 seconds, check again")
        return
    rec = data.get("last_game") or data["current"]
    print()
    where = "in Game Mode" if rec["game_mode"] else "in Desktop Mode"
    print(color(1, f"{'While playing ' + rec['game'] if rec['game'] else 'Last update'} ({ago(rec['time'])}, {where}):"))
    print(f"      {rec['power']}, {rec['screen']}, {rec.get('input', '')}")
    labels = [("tdp", "Power limit"), ("gpu", "GPU clock"), ("fps", "FPS cap"), ("refresh", "Refresh rate"),
              ("smt", "Hyperthreading"), ("cpu_boost", "CPU boost"), ("cpu_epp", "CPU power preference"),
              ("vrr", "VRR (adaptive sync)"), ("allow_tearing", "Tearing")]
    for key, label in labels:
        r = rec["results"].get(key)
        if r:
            line(r["ok"], f"{label}: {r['detail']}")
    for p in rec["problems"]:
        line(False, p)
    if data.get("memory"):
        line(data["memory"]["ok"], f"Memory tweaks: {data['memory']['detail']}")
    if data.get("polling"):
        line(data["polling"]["ok"], f"Controller polling: {data['polling']['detail']}")
    for b in controller_batteries():  # live, not from the record
        amount = f"{b['percent']}%" if b["percent"] is not None else (b["level"] or "unknown").lower()
        extra = " (charging)" if b["charging"] else " - low, charge it soon" if battery_is_low(b) else ""
        line(not battery_is_low(b), f"Controller battery: {b['name']} {amount}{extra}")
    if rec["game"] and not rec["game_mode"]:
        line(False, "Game Mode wasn't detected, so the FPS cap and refresh rate were skipped")
    if not data.get("last_game"):
        line(None, "No game has been played since the service started")

    if data.get("graphics"):
        print()
        print(color(1, "Game settings (written into the game's own config, for its next launch):"))
        for g in data["graphics"].values():
            line(g["ok"], f"{g['game']}: {g['detail']} ({ago(g['time'])})")

    if data.get("events"):
        print()
        print(color(1, "Recent changes:"))
        for e in data["events"][-6:]:
            print(f"      {e}")


def cmd_graphics_restore(args):
    """Puts the games' own settings files back as they were before DeckOpt touched them (root)."""
    done = Graphics.restore_all()
    if not done:
        print("No game settings to put back.")
    for _, path, err in done:
        print(f"{'FAILED' if err else 'restored'}: {path}" + (f" ({err})" if err else ""))
    sys.exit(1 if any(err for _, _, err in done) else 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("command", nargs="?", default="status",
                        choices=["run", "status", "apply", "check", "graphics-restore"],
                        help="run: background service | status: show what would be applied | apply: apply once | "
                             "check: health check | graphics-restore: undo the in-game settings changes")
    parser.add_argument("-c", "--config", default=default_config())
    parser.add_argument("-n", "--dry-run", action="store_true", help="log changes instead of writing them")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO, format="%(levelname)s %(message)s")

    if args.command == "run":
        cmd_run(args)
    elif args.command == "check":
        cmd_check(args)
    elif args.command == "graphics-restore":
        cmd_graphics_restore(args)
    else:
        cmd_status(args, apply=args.command == "apply")


if __name__ == "__main__":
    main()
