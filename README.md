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

## Install

On the Deck, switch to **Desktop Mode**, open **Konsole**, and paste:

```sh
curl -fsSL https://raw.githubusercontent.com/NoahJ456-007/DeckOpt/main/install.sh | bash
```

That's it. It asks for your password (and helps you create one if you never have), installs
the background service, and puts three shortcuts on your desktop:

| Shortcut | What it does |
|---|---|
| **Update DeckOpt** | Gets the latest version from GitHub (your profiles are kept) |
| **DeckOpt Check** | Plain-English health check: did each setting actually apply during your last game, or did Steam override it? |
| **Edit DeckOpt Profiles** | Opens your profiles file |

Go back to Game Mode and play. It keeps running across reboots and SteamOS updates.

**Using the shortcuts from Game Mode:** in Desktop Mode, open Steam → *Add a Game → Add a
Non-Steam Game* and tick "Update DeckOpt" and "DeckOpt Check".

## Update

Double-click **Update DeckOpt**, or run `deckopt update`. If you're already on the latest
version it says so and stops. Your `profiles.toml` is never overwritten. The newest example
profiles are saved next to it as `profiles.example.toml` so you can copy in anything new.

**Important:** in the Quick Access menu → Performance, leave **TDP limit, GPU clock, frame limit and
refresh rate** unchanged (and turn off "Use per-game profile"). Otherwise Steam and DeckOpt will
overwrite each other. DeckOpt applies once more a few seconds after a game launches to
catch Steam's own launch-time settings.

## Your profiles

Edit `~/.config/deckopt/profiles.toml` (or run `deckopt edit`). Changes apply live. Layers stack from general to
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
deckopt status
```

```
game: ELDEN RING (1245620) | power: charger | screen: LG TV SSCR2 (DP-1 1920x1080@60Hz)
  -> tdp 15W, gpu auto, fps 30, smt on, boost on
  layers: defaults > profile > profile.external > game."1245620" > game."1245620".external
```

## Commands

| | |
|---|---|
| `deckopt check` | Health check (✔/✘ per setting, and what to fix) |
| `deckopt status` | Show what's detected and what's being applied |
| `deckopt update` | Update to the latest version |
| `deckopt edit` | Open your profiles |
| `deckopt log` | Watch it switch profiles live |
| `deckopt restart` | Restart the service |
| `deckopt version` | Show the installed version |
| `deckopt uninstall` | Remove it (keeps your profiles) |

Open a new Konsole window after installing so the `deckopt` command is found.

## Something not working?

Double-click **DeckOpt Check** in Desktop Mode. It shows what happened during your last game: each setting
is marked OK or BAD, with the reason and the fix. You don't need to be in the game when you run it.

## Notes

- FPS limit and refresh rate go through gamescope, so they only work in **Game Mode**. TDP, GPU and CPU settings work in both modes.
- Stopping the service puts everything back to stock (15 W, auto GPU, no frame cap).
- TDP is capped at 15 W, the Deck's stock maximum.
