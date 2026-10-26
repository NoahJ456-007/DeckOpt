# DeckOpt

Automatic per-game tuning for the Steam Deck. It runs in the background and re-tunes the
Deck the moment any of these change:

| It watches | How it knows |
|---|---|
| **The game you're playing** | Steam's `reaper SteamLaunch AppId=…` process; the name comes from the game's appmanifest (internal storage + SD card) |
| **Charger or battery** | `/sys/class/power_supply` (AC adapter, plus battery % for a low-battery mode) |
| **The screen you're on** | DRM connectors: built-in panel vs. an external monitor/TV, read from the screen's EDID (name, resolution, refresh rate) |

and sets:

- **TDP** (3–15 W)
- **GPU clock** (auto, or locked 200–1600 MHz)
- **FPS limit**: a number, `"refresh"` (match the screen) or `"half"`
- **Built-in screen refresh rate**: `"fps"` picks the lowest rate that's a whole multiple of the frame cap (40 fps → 40 Hz, 30 fps → 60 Hz) for smooth pacing on minimal power. It knows the LCD (40–60 Hz) and the OLED (45–90 Hz) apart.
- **SMT** (hyperthreading) and **CPU boost**

When docked, the frame cap follows the TV/monitor's own refresh rate, and you can give a specific
screen its own profile by name.

## Install (on the Deck)

1. Copy the `DeckOpt` folder to the Deck, using a USB stick, `scp`, or KDE Connect.
2. Switch to **Desktop Mode** and open **Konsole**.
3. If you've never set a sudo password: `passwd`
4. Run:
   ```sh
   cd ~/DeckOpt        # wherever you copied it
   sudo ./install.sh
   ```

It installs to `/var/lib/deckopt` (survives SteamOS updates) and starts a systemd service.
Go back to Game Mode and play.

**Important:** in the Quick Access menu → Performance, leave **TDP limit, GPU clock, frame limit and
refresh rate** unchanged (and turn off "Use per-game profile"). Otherwise Steam and DeckOpt will
overwrite each other. DeckOpt applies once more a few seconds after a game launches to
catch Steam's own launch-time settings.

## Your profiles

Edit `~/.config/deckopt/profiles.toml`. Changes apply live. Layers stack from general to
specific, and later ones win:

```
profile  →  display."<screen>"  →  game.<AppId or name>  →  low_battery
```

Each layer can have `.battery`, `.ac`, `.internal`, `.external`, `."battery+internal"` (etc.)
and `.low_battery` sub-tables. Example:

```toml
[game.1245620]          # Elden Ring
fps = 40
refresh = "fps"         # 40fps @ 40Hz on the built-in screen
[game.1245620.battery]
tdp = 13
fps = 30
[game.1245620.external]
fps = 30

[display."LG TV SSCR2"] # any game on this TV
fps = 60
```

To find a game's AppId and your screen's name, start the game and run in Konsole:

```sh
deckopt status          # or: python3 /var/lib/deckopt/deckopt.py status
```

```
game: ELDEN RING (1245620) | power: charger | screen: LG TV SSCR2 (DP-1 1920x1080@60Hz)
  -> tdp 15W, gpu auto, fps 30, smt on, boost on
  layers: defaults > profile > profile.external > game."1245620" > game."1245620".external
```

## Commands

| | |
|---|---|
| `deckopt status` | Show what's detected and what would be applied |
| `deckopt apply` | Apply once now |
| `deckopt run` | The background loop (the service runs this) |
| `-n / --dry-run` | Log what would be written without touching anything |
| `journalctl -u deckopt -f` | Watch it switch profiles live |
| `sudo ./install.sh --uninstall` | Remove it (keeps your profiles) |

## Notes

- FPS limit and refresh rate go through gamescope, so they only work in **Game Mode**. TDP, GPU and CPU settings work in both modes.
- Stopping the service puts everything back to stock (15 W, auto GPU, no frame cap).
- TDP is capped at 15 W, the Deck's stock maximum.
