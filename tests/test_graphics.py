#!/usr/bin/env python3
"""Self-contained test for DeckOpt's in-game graphics settings (stdlib only).

Builds a fake Steam library tree (main library + SD card, libraryfolders.vdf, appmanifest, Proton prefix) with
a UTF-16 LE BOM, CRLF Path of Exile 2 config built from a real dump, then checks that DeckOpt finds the right
file, changes only keys that exist, keeps every other byte, keeps the file's mode, backs up and restores
byte-exact, and never writes while the game is running.  Run:  python3 tests/test_graphics.py
"""

import importlib.util
import os
import shutil
import stat
import sys
import tempfile
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ROOT = Path(tempfile.mkdtemp(prefix="deckopt-test-"))
os.environ["DECKOPT_SYSROOT"] = str(ROOT)
os.environ["DECKOPT_BACKUPS"] = str(ROOT / "backups")
os.environ["DECKOPT_STATE"] = str(ROOT / "state.json")

spec = importlib.util.spec_from_file_location("deckopt", REPO / "deckopt.py")
deckopt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deckopt)

APPID = "2694490"
MAIN = ROOT / "home/deck/.local/share/Steam"
SD = ROOT / "run/media/deck/SDCARD"
PREFIX = SD / "steamapps/compatdata" / APPID / "pfx/drive_c/users/steamuser"
CONFIG = PREFIX / "Documents/My Games/Path of Exile 2/poe2_production_Config.ini"

# From real dumps (github.com/Lailloken/Exile-UI/issues/717, pathofexile.com/forum/view-thread/3750303), with
# comments and odd spacing added to prove they survive.
POE2_INI = "\r\n".join([
    "; written by the game",
    "[LOGIN]",
    "gateway_auto_select=false",
    "seen_intro=true",
    "",
    "[DISPLAY]",
    "resolution_width=1280",
    "resolution_height=800",
    "renderer_type=Vulkan",
    "shadow_type=High",
    "vsync=Adaptive",
    "use_dynamic_particle_culling2=false",
    "screenspace_effects=2",
    "light_quality = 2",            # spacing around = must be kept
    "texture_filtering=8",
    "upscale_quality=Quality",
    "# dynamic resolution",
    "use_dynamic_resolution=false",
    "dynamic_resolution_fps=60",
    "water_detail=1",
    "global_illumination_detail=1",
    "texture_quality=TextureQualityHigh",
    "framerate_limit=240",
    "framerate_limit_enabled=false",
    "background_framerate_limit=30",
    "background_framerate_limit_enabled=false",
    "[SOUND]",
    "master_volume2=100",
    "shadow_type=NotTheDisplayOne",  # same key name in another section: must not be touched
    "",
]).encode("utf-16-le")
POE2_INI = b"\xff\xfe" + POE2_INI

passed = 0


def check(cond, what):
    global passed
    if not cond:
        print(f"FAIL: {what}")
        sys.exit(1)
    passed += 1
    print(f"  ok  {what}")


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else data.encode())


def display_values(data):
    """[DISPLAY] key -> value from the file as the game would read it."""
    text = data[2:].decode("utf-16-le") if data.startswith(b"\xff\xfe") else data.decode("utf-8-sig")
    out, section = {}, ""
    for line in text.splitlines():
        if line.startswith("["):
            section = line.strip("[]")
        elif "=" in line and section == "DISPLAY":
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out


def state(ac, internal=True, kind="built-in"):
    screen = deckopt.Screen("eDP-1" if internal else "HDMI-A-1", internal, "Built-in" if internal else "LG TV",
                            kind=kind)
    return deckopt.State(None, None, ac, 55, False, screen, "deck")


class Names:
    def get(self, appid):
        return {APPID: "Path of Exile 2", "999": "Fake Unreal Game"}.get(appid)


# --- fake Steam tree ----------------------------------------------------------
write(MAIN / "steamapps/libraryfolders.vdf", f'''"libraryfolders"
{{
	"0"
	{{
		"path"		"/home/deck/.local/share/Steam"
		"apps"
		{{
		}}
	}}
	"1"
	{{
		"path"		"/run/media/deck/SDCARD"
		"apps"
		{{
			"{APPID}"		"44000000000"
		}}
	}}
}}
''')
write(SD / "steamapps" / f"appmanifest_{APPID}.acf",
      f'"AppState"\n{{\n\t"appid"\t\t"{APPID}"\n\t"name"\t\t"Path of Exile 2"\n\t"installdir"\t\t"Path of Exile 2"\n}}\n')
# A decoy prefix in the main library: the game is NOT installed there, so it must never be edited.
decoy = MAIN / "steamapps/compatdata" / APPID / "pfx/drive_c/users/steamuser/Documents/My Games/Path of Exile 2/poe2_production_Config.ini"
write(decoy, POE2_INI)

cfg = tomllib.loads((REPO / "profiles.toml").read_text())
cfg = deckopt.deep_merge(cfg, {"settings": {"user": "deck"},
                               "graphics": {APPID: {"values": {"DISPLAY": {"made_up_key": 1}}}}})
print("Test: shipped Path of Exile 2 rule against a UTF-16 LE BOM, CRLF config")

# --- 1. before first launch -----------------------------------------------------
g = deckopt.Graphics(cfg)
g.apply_all(cfg, state(ac=False), Names())
r = g.results[APPID]
check(r["ok"] is None and "first launch" in r["detail"], f"no file yet -> waiting: {r['detail']}")
check(not (ROOT / "backups").exists(), "no backup made while waiting")

# --- 2. first launch has happened: file exists (also via the old "My Documents" symlink) -----------------
write(CONFIG, POE2_INI)
os.chmod(CONFIG, 0o640)
if os.geteuid() == 0:  # as root (like the service): make it the user's file so the chown is really tested
    import pwd
    try:
        pw = pwd.getpwnam("deck")
        os.chown(CONFIG, pw.pw_uid, pw.pw_gid)
    except KeyError:
        pass
os.symlink("Documents", PREFIX / "My Documents")
before = CONFIG.stat()

g.apply_all(cfg, state(ac=False), Names())
r = g.results[APPID]
check(r["ok"] is True, f"applied on battery: {r['detail']}")
check("made_up_key" in r["detail"] and "skipped" in r["detail"], "missing key reported as skipped")
after = CONFIG.read_bytes()
check(after.startswith(b"\xff\xfe"), "UTF-16 LE BOM kept")
check(b"\r\x00\n\x00" in after and b"\n\x00" not in after.replace(b"\r\x00\n\x00", b""), "CRLF line endings kept")
check(decoy.read_bytes() == POE2_INI, "decoy copy in the other library untouched")
values = display_values(after)
check(values["shadow_type"] == "Low", "shadow_type -> Low")
check(values["texture_quality"] == "TextureQualityLow", "texture_quality -> TextureQualityLow (battery)")
check(values["dynamic_resolution_fps"] == "30" and values["use_dynamic_resolution"] == "true",
      "dynamic resolution on, aiming at 30 (battery)")
check(values["framerate_limit"] == "30" and values["framerate_limit_enabled"] == "true", "in-game cap 30 (battery)")
check(values["use_dynamic_particle_culling2"] == "true", "dynamic culling on")
check(values["vsync"] == "Off", "vsync -> Off")
check("made_up_key" not in values, "missing key was not added")
check(values["resolution_width"] == "1280" and values["renderer_type"] == "Vulkan", "unmanaged keys untouched")

old_lines = POE2_INI[2:].decode("utf-16-le").split("\r\n")
new_lines = after[2:].decode("utf-16-le").split("\r\n")
check(len(old_lines) == len(new_lines), "same number of lines (nothing added or removed)")
changed = [(o, n) for o, n in zip(old_lines, new_lines) if o != n]
check(all(o.split("=")[0] == n.split("=")[0] for o, n in changed), "only values changed, keys and order intact")
check(("light_quality = 2", "light_quality = 0") in changed, "spacing around = kept on changed line")
check([n for o, n in changed if n.startswith(("#", ";"))] == [], "comments intact")
check("shadow_type=NotTheDisplayOne" in new_lines, "same-named key in another section not touched")
check(all(k in POE2_INI[2:].decode("utf-16-le") for k, _ in [(o.split("=")[0], 0) for o, _ in changed]),
      f"{len(changed)} lines changed, all pre-existing keys")

st = CONFIG.stat()
check(stat.S_IMODE(st.st_mode) == 0o640, "file mode 0640 kept")
check((st.st_uid, st.st_gid) == (before.st_uid, before.st_gid), "owner and group kept")
check(not list(CONFIG.parent.glob(".*deckopt*")), "no temp file left behind")

backup = ROOT / "backups" / APPID / CONFIG.name
check(backup.read_bytes() == POE2_INI, "backup is the untouched original")
check(backup.with_name(backup.name + ".path").read_text() == str(CONFIG), "backup remembers where it came from")

# --- 3. plugged in: the charger layer, backup untouched ------------------------------------------------
g.apply_all(cfg, state(ac=True, internal=False, kind="tv"), Names())
values = display_values(CONFIG.read_bytes())
check(values["texture_quality"] == "TextureQualityMedium" and values["dynamic_resolution_fps"] == "40",
      "charger + TV: medium textures, dynamic resolution aiming at 40")
check(values["shadow_type"] == "Low", "charger: shadows still Low")
check(backup.read_bytes() == POE2_INI, "backup still the original after a second apply")
second = CONFIG.read_bytes()
g.apply_all(cfg, state(ac=True, internal=False, kind="tv"), Names())
check(CONFIG.read_bytes() == second, "re-applying the same state rewrites nothing (bytes identical)")

# --- 4. never while the game runs ------------------------------------------------------------------
proc = ROOT / "proc/4242"
write(proc / "cmdline", b"reaper\0SteamLaunch\0AppId=" + APPID.encode() + b"\0--\0PathOfExile.exe\0")
write(proc / "comm", b"reaper\n")
g.apply_all(cfg, state(ac=False), Names())
check(CONFIG.read_bytes() == second, "game running: apply_all writes nothing")
done = deckopt.Graphics.restore_all()
check(done and done[0][2] and "running" in done[0][2] and CONFIG.read_bytes() == second,
      "game running: restore refuses and writes nothing")
shutil.rmtree(proc)

# --- 5. restore round-trips byte-exact -------------------------------------------------------------
done = deckopt.Graphics.restore_all()
check(done == [(APPID, CONFIG, None)], f"restore reports success: {done}")
check(CONFIG.read_bytes() == POE2_INI, "restored file is byte-identical to the original")
check(stat.S_IMODE(CONFIG.stat().st_mode) == 0o640, "mode kept through restore")
check(not (ROOT / "backups" / APPID).exists(), "backup removed after restore")
check(deckopt.Graphics.restore_all() == [], "nothing left to restore")

# --- 6. plain UTF-8, LF, no BOM; and the Unreal preset's discovery --------------------------------------
print("Test: Unreal preset (UTF-8 BOM, CRLF GameUserSettings.ini found by wildcard)")
write(MAIN / "steamapps/appmanifest_999.acf", '"AppState"\n{\n\t"appid"\t\t"999"\n\t"name"\t\t"Fake Unreal Game"\n\t"installdir"\t\t"Fake"\n}\n')
ue = MAIN / "steamapps/compatdata/999/pfx/drive_c/users/steamuser/AppData/Local/FakeGame/Saved/Config/WindowsNoEditor/GameUserSettings.ini"
UE_INI = "﻿[ScalabilityGroups]\r\nsg.ResolutionQuality=100.000000\r\nsg.ViewDistanceQuality=3\r\nsg.ShadowQuality=3\r\nsg.TextureQuality=3\r\n\r\n[/Script/Engine.GameUserSettings]\r\nbUseVSync=False\r\n"
write(ue, UE_INI.encode("utf-8"))
cfg2 = deckopt.deep_merge(cfg, {"graphics": {"999": {"preset": "unreal"}}})
g = deckopt.Graphics(cfg2)
g.apply_all(cfg2, state(ac=False), Names())
r = g.results["999"]
check(r["ok"] is True, f"unreal preset applied: {r['detail']}")
data = ue.read_bytes()
check(data.startswith(b"\xef\xbb\xbf") and b"\r\n" in data, "UTF-8 BOM and CRLF kept")
text = data.decode("utf-8-sig")
check("sg.ShadowQuality=0" in text and "sg.TextureQuality=0" in text and "sg.ViewDistanceQuality=1" in text,
      "sg.* values set from the template's battery layer")
check("sg.ResolutionQuality=100.000000" in text and "bUseVSync=False" in text, "unmanaged keys untouched")
check("sg.GlobalIlluminationQuality" not in text and "GlobalIllumination" in r["detail"], "missing sg.* key skipped, not added")
done = deckopt.Graphics.restore_all()
check(ue.read_bytes() == UE_INI.encode("utf-8"), "unreal file restored byte-exact")

# --- 7. IniFile on its own: plain UTF-8 LF without BOM, and a BOM-less UTF-16 file -------------------------
doc = deckopt.IniFile(b"[A]\nx=1\ny = 2\n; c\n[B]\nx=9")
check(doc.set("a", "X", "5") and doc.set("B", "x", "9") is True and not doc.set("A", "nope", "1"), "case-insensitive match, missing key refused")
check(doc.dump() == b"[A]\nx=5\ny = 2\n; c\n[B]\nx=9", "UTF-8 LF output, no trailing newline added")
raw = "[A]\r\nx=1\r\n".encode("utf-16-le")
doc = deckopt.IniFile(raw)
doc.set("A", "x", "2")
check(doc.dump() == "[A]\r\nx=2\r\n".encode("utf-16-le"), "UTF-16 without BOM detected and kept BOM-less")

shutil.rmtree(ROOT)
print(f"\nAll {passed} checks passed.")
