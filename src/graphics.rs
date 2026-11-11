//! In-game graphics settings (TODO: not ported yet).
//!
//! A separate branch of the Python version adds a feature that edits games' own config files (for
//! example Path of Exile 2's production_Config.ini) when a game is detected, so in-game quality settings
//! follow the DeckOpt profile. This module is where that feature lands when it's ported:
//!
//! * Config: a `[game.<id>.graphics]` sub-table (or whatever the Python branch settles on) carried
//!   through `profile::resolve` untouched; this module reads it from the resolved game table.
//! * Hook: `service::run` calls [`apply`] right after `tuner.apply(final)` whenever the state key changes
//!   and a game is running (see the `graphics::apply` call site there).
//! * Rules: edit files as the deck user (`Tuner::run_as_user`, or chown after writing as root), keep a
//!   `.deckopt.bak` copy for `restore`, never touch a file while the game has it open for writing, and
//!   record a `Note` under `results["graphics"]` so `deckopt check` shows it.
//!
//! Until then it's a no-op so the rest of the service doesn't need to change when it arrives.

use crate::config::Config;
use crate::detect::State;
use crate::tuner::{Note, Tuner};

/// Apply the game's graphics settings for `state`, if any are configured. Currently a no-op.
pub fn apply(_cfg: &Config, _state: &State, _tuner: &mut Tuner) -> Option<Note> {
    None
}

/// Put edited config files back. Currently a no-op.
pub fn restore(_tuner: &mut Tuner) {}
