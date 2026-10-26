# DeckOpt

Automatic per-game tuning for the Steam Deck. It runs in the background and re-tunes the
Deck the moment any of these change:

| It watches | How it knows |
|---|---|
| **The game you're playing** | Every game Steam launches, including non-Steam games and emulators added to Steam. Names come from Steam's app manifests and `shortcuts.vdf` |
| **Charger or battery** | The AC adapter, plus battery % for a low-battery mode |
| **The screen you're on** | Built-in panel, **TV** or **monitor**, with the resolution and refresh rate it's *actually* running at (not just what the screen advertises) |
| **What you're playing with** | The Deck's own controls, a **controller** (Xbox, PlayStation, Switch, 8BitDo...), or **keyboard and mouse**. Keyboard/mouse counts while you're using it; a connected controller counts otherwise. Controllers are never read, so Steam Input keeps them to itself |

### Every game is optimized, profile or not

- **Adaptive 60 FPS on the charger.** DeckOpt reads the game's real frame times from Game Mode (the same
  data as Steam's performance overlay). If the game holds 60, it stays at 60. If it can't, the cap
  steps down to the next evenly paced level (60 → 45 → 40 → 30 on the built-in screen, 60 → 30 on a
  60Hz TV), so you get smooth, steady frames instead of a stuttery 52. When the game runs comfortably
  with GPU to spare it tries the next level up, backing off for longer each time that doesn't hold.
  Where each game settles is remembered. (Needs a SteamOS with gamescope 3.16.20 or newer; on older
  versions the cap simply stays at 60.)
- **Automatic power tuning on battery.** DeckOpt watches GPU and CPU load. If the game is easily
  hitting its frame cap, the power limit steps down until it's just keeping up; if the GPU or a CPU
  core maxes out, it steps back up fast. The result is remembered per game, power source and screen,
  so the next session starts tuned.
- **Smooth frame pacing on the built-in screen.** The refresh rate is set to an even multiple of the
  frame cap: 40 FPS → 40Hz on the LCD, 60 FPS → 60Hz (not a juddery 90Hz) on the OLED.
- Hand-tuned profiles for some games (Elden Ring, Cyberpunk, Hades, Stardew, Terraria, Cuphead).
  Updates add more, and your own settings always win.

### Optimized for whatever it's plugged into

- **TVs and monitors are told apart**: "TV" in the name, a big screen (80cm+), or HDMI data with no
  fixed size (TVs, projectors). Give each kind its own settings with `[profile.tv]` / `[profile.monitor]`.
- **The frame cap fits the screen's refresh rate evenly**, so every frame is shown for the same time:
  60Hz TV → 60, 120Hz TV → 60, 144Hz monitor → 48, 165Hz → 55. A game set to 30 stays a steady 30.
- **It follows the live mode.** If you set a 144Hz monitor to 60Hz in Steam, the cap follows.
- Any one screen can get its own profile by name.

### Optimized for how you're playing

- **Keyboard and mouse on the charger** run up to the screen's refresh rate (144 FPS on a 144Hz
  monitor) for the most responsive aim. Controllers keep the evenly paced cap.
- Give each input its own settings with `[profile.controller]`, `[profile.kbm]`, `[profile.deck]`,
  or combine with power like `[profile."ac+kbm"]`.

### Extras (ideas from CryoUtilities, SimpleDeckyTDP and PowerTools)

- **CPU power preference** (SteamOS 3.7.5+): saves power on battery, favours speed on the charger,
  and goes to maximum saving when the battery is low.
- **Memory tweaks**: CryoUtilities' recommended settings (swappiness, huge pages, compaction, page
  lock fairness), which can smooth out stutter in some games. Your original values come back if
  DeckOpt stops. Turn off with `memory_tweaks = false`.
- **Re-applies everything after sleep**, since some settings reset when the Deck wakes up.
- **Controller battery alerts**: DeckOpt Check shows your controller's battery (PlayStation, Bluetooth
  Xbox, Switch controllers) and flags it when it's low.
- **Faster polling for wired PlayStation and generic USB controllers** (1000Hz instead of 125-250Hz),
  for less input lag. Xbox-style pads already poll fast.
- **Lower lag on TVs and monitors**: adaptive sync (VRR) is switched on when the screen supports it.
  Tearing (lowest lag, visible tear lines) is available with `allow_tearing = true`.
- **Profiles per controller**: `[controller."GameSir"]` or `[controller."3537"]` (vendor id) settings
  apply while that controller is in use.
- **Keeps wireless controller dongles awake**: turns off USB power saving for them, a common cause of
  controllers dropping and reconnecting. Restored when DeckOpt stops.
- **Optional docked charge limit**: set `docked_charge_limit = 80` and the battery stops at 80% while
  you're docked, which is easier on it long term. Your normal limit comes back when you undock. Off by default.

### What it controls

- **Power limit (TDP)**, 3–15 W, fixed or auto-tuned
- **GPU clock**: auto, or locked at 200–1600 MHz
- **FPS limit**, sent to Game Mode the same way Steam's framerate slider does, which also sets the built-in screen's refresh rate to match (it knows the LCD's 40–60Hz from the OLED's 45–90Hz)
- **SMT** (hyperthreading), **CPU boost** and **CPU power preference**
- **Memory settings** and, optionally, the **battery charge limit** while docked

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
| **DeckOpt On-Off** | Turns DeckOpt off (stock settings come back) or on again. Handy to see if something is DeckOpt's doing |

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

You don't have to set anything up: the built-in profiles cover every game and screen. To change
something, double-click **Edit DeckOpt Profiles** (`~/.config/deckopt/profiles.toml`). It only needs
the settings you want to override, and changes apply live. `profiles.example.toml` next to it lists
every built-in profile and option. Layers stack from general to
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
