#!/bin/bash
# DeckOpt installer / updater for SteamOS. Paste this into Konsole (Desktop Mode):
#
#   curl -fsSL https://raw.githubusercontent.com/NoahJ456-007/DeckOpt/main/install.sh | bash
#
# After that:  deckopt update   |   deckopt uninstall   (or use the desktop shortcuts)
#
# DeckOpt is a single static program (`deckopt`, built from the Rust sources in src/). The installer
# downloads the release build for this machine and checks its SHA-256 before installing it. If no
# release build can be downloaded, it falls back to the Python version (deckopt.py) when python3 is
# available, and says so.
set -euo pipefail

REPO=NoahJ456-007/DeckOpt
APP_DIR=/var/lib/deckopt          # /var and /etc survive SteamOS updates
UNIT=/etc/systemd/system/deckopt.service
GITHUB=${DECKOPT_GITHUB:-https://github.com}          # overridable for offline testing
GITHUB_API=${DECKOPT_GITHUB_API:-https://api.github.com}
ARCH=$(uname -m)
ASSET="deckopt-${ARCH}-unknown-linux-musl"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m!!\033[0m %s\n' "$*" >&2; exit 1; }

# Where this script's files are. Empty-ish when piped from curl, so we download them.
SRC=$(cd "$(dirname "${BASH_SOURCE[0]:-.}")" 2>/dev/null && pwd || pwd)
CMD=${1:-install}
TAG=""

have_program() { [[ -f $1/deckopt && -s $1/deckopt ]]; }

latest_tag() {
    # The newest release tag (e.g. v2.0.0), or empty if there are no releases or we're offline.
    curl -fsSL "$GITHUB_API/repos/$REPO/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' | cut -d'"' -f4 || true
}

fetch_latest() {
    local tmp
    tmp=$(mktemp -d)
    say "Downloading the latest DeckOpt..."
    TAG=$(latest_tag)
    if [[ -n $TAG ]]; then
        # Support files (profiles, service, this script) from the same tag as the program.
        curl -fsSL "$GITHUB/$REPO/archive/refs/tags/$TAG.tar.gz" | tar xz -C "$tmp" --strip-components=1 \
            || die "Download failed. Check your internet connection."
        echo "$TAG" > "$tmp/VERSION"
    else
        curl -fsSL "$GITHUB/$REPO/archive/refs/heads/main.tar.gz" | tar xz -C "$tmp" --strip-components=1 \
            || die "Download failed. Check your internet connection."
        curl -fsSL "$GITHUB_API/repos/$REPO/commits/main" 2>/dev/null \
            | grep -m1 '"sha"' | cut -d'"' -f4 | cut -c1-7 > "$tmp/VERSION" || true
    fi
    SRC=$tmp
    fetch_program "$SRC" || true
}

fetch_program() {
    # Downloads the release build of `deckopt` for this machine into $1 and verifies its checksum.
    # Returns 1 (after explaining) if that isn't possible; the caller decides on a fallback.
    local dir=$1 base sum
    [[ -n $TAG ]] || TAG=$(latest_tag)
    if [[ -n $TAG ]]; then
        base="$GITHUB/$REPO/releases/download/$TAG/$ASSET"
    else
        base="$GITHUB/$REPO/releases/latest/download/$ASSET"
    fi
    say "Downloading the DeckOpt program for $ARCH..."
    if ! curl -fsSL -o "$dir/deckopt.download" "$base" || ! curl -fsSL -o "$dir/deckopt.sha256" "$base.sha256"; then
        warn "No release build of DeckOpt for $ARCH could be downloaded from $base"
        rm -f "$dir/deckopt.download" "$dir/deckopt.sha256"
        return 1
    fi
    sum=$(cut -d' ' -f1 "$dir/deckopt.sha256")
    if [[ $(sha256sum "$dir/deckopt.download" | cut -d' ' -f1) != "$sum" ]]; then
        warn "The downloaded DeckOpt program failed its checksum (corrupt download?), not using it."
        rm -f "$dir/deckopt.download" "$dir/deckopt.sha256"
        return 1
    fi
    mv "$dir/deckopt.download" "$dir/deckopt"
    chmod 755 "$dir/deckopt"
    say "Program verified (SHA-256 OK)."
}

ensure_sudo_password() {
    # A fresh Deck has no password, and sudo needs one.
    local status answer=y
    sudo -n true 2>/dev/null && return
    status=$(passwd -S "$USER" 2>/dev/null | awk '{print $2}')
    if [[ -z $status ]]; then
        read -rp "Have you already set a password on this Deck? [y/n] " answer </dev/tty
    fi
    if [[ $status == NP || $answer == [nN]* ]]; then
        say "SteamOS needs a password for system changes and you haven't set one yet."
        say "Pick one now. You'll type it twice, and nothing shows while you type."
        passwd </dev/tty
    fi
}

uninstall() {
    local user=${SUDO_USER:-deck} home
    home=$(getent passwd "$user" | cut -d: -f6)
    systemctl disable --now deckopt.service 2>/dev/null || true
    rm -rf "$APP_DIR" "$UNIT"
    rm -f "$home/.local/bin/deckopt" "$home"/.local/share/applications/deckopt-*.desktop "$home"/Desktop/deckopt-*.desktop
    systemctl daemon-reload
    say "DeckOpt removed. Your profiles in $home/.config/deckopt were kept."
}

install_program_as_root() {
    # The program itself: the verified release build, or the Python version as a fallback.
    if ! have_program "$SRC"; then
        fetch_program "$SRC" || true
    fi
    if have_program "$SRC"; then
        install -m 755 "$SRC/deckopt" "$APP_DIR/deckopt"
        rm -f "$APP_DIR/deckopt.py"   # from an older, Python-based install
        return
    fi
    if [[ -f $APP_DIR/deckopt ]] && ! head -c 2 "$APP_DIR/deckopt" | grep -q '#!'; then
        warn "Keeping the DeckOpt program that's already installed; only the profiles and scripts were updated."
        return
    fi
    if [[ -f $SRC/deckopt.py ]] && command -v python3 >/dev/null; then
        warn "Using the Python version of DeckOpt instead (same features, needs python3, which SteamOS has)."
        warn "Run 'deckopt update' later to get the release build once it's available."
        install -m 755 "$SRC/deckopt.py" "$APP_DIR/deckopt.py"
        printf '#!/bin/bash\nexec /usr/bin/python3 %s/deckopt.py "$@"\n' "$APP_DIR" > "$APP_DIR/deckopt"
        chmod 755 "$APP_DIR/deckopt"
        return
    fi
    die "Couldn't get the DeckOpt program. Check your internet connection and try again, or download $ASSET from https://github.com/$REPO/releases, put it next to install.sh as 'deckopt', and run install.sh again."
}

install_as_root() {
    local user=${SUDO_USER:-deck} home config_dir
    home=$(getent passwd "$user" | cut -d: -f6)
    config_dir=$home/.config/deckopt

    say "Installing service..."
    install -d -m 755 "$APP_DIR"
    install_program_as_root
    install -m 755 "$SRC/install.sh" "$APP_DIR/"
    install -m 644 "$SRC/profiles.toml" "$SRC/deckopt.service" "$APP_DIR/"
    [[ -s $SRC/VERSION ]] && install -m 644 "$SRC/VERSION" "$APP_DIR/VERSION"
    sed "s|@CONFIG@|$config_dir/profiles.toml|; s|@APP_DIR@|$APP_DIR|" "$SRC/deckopt.service" > "$UNIT"

    # Profiles belong to the user. Never overwrite their edits; keep the latest examples beside them.
    sudo -u "$user" bash -s "$SRC" "$config_dir" "$home" <<'USER'
set -e
src=$1 config_dir=$2 home=$3
mkdir -p "$config_dir" "$home/.local/bin" "$home/.local/share/applications"
# Older versions copied the full defaults here, which would shadow every improvement in the built-in
# profiles. If that copy was never edited (it matches a shipped version), swap in the short overrides file.
SHIPPED_DEFAULTS="43941b2b5f2b96aa4c88e448e2cca9e891ce6be33dbabf7d034c7beeaac19165
cedd06095073167342ee6c8e93aa91da54597b87168a1dedd0df3d4726621e32"
if [[ -f $config_dir/profiles.toml ]] &&
   grep -qx "$(sha256sum "$config_dir/profiles.toml" | cut -d' ' -f1)" <<<"$SHIPPED_DEFAULTS"; then
    rm "$config_dir/profiles.toml"
fi
[[ -f $config_dir/profiles.toml ]] || cp "$src/my-profiles.toml" "$config_dir/profiles.toml"
cp "$src/profiles.toml" "$config_dir/profiles.example.toml"

cat > "$home/.local/bin/deckopt" <<EOF
#!/bin/bash
APP=/var/lib/deckopt
CONFIG=$config_dir/profiles.toml
case "\${1:-status}" in
    update)    exec bash "\$APP/install.sh" update ;;
    uninstall) exec sudo bash "\$APP/install.sh" uninstall ;;
    edit)      exec xdg-open "\$CONFIG" ;;
    log|logs)  exec sudo journalctl -u deckopt -f -o cat ;;
    restart)   exec sudo systemctl restart deckopt ;;
    toggle)    if systemctl is-active --quiet deckopt; then
                   sudo systemctl stop deckopt && echo "DeckOpt is OFF: stock settings restored until you turn it on or restart the Deck."
               else
                   sudo systemctl start deckopt && echo "DeckOpt is ON."
               fi ;;
    version)   cat "\$APP/VERSION" 2>/dev/null || echo unknown ;;
    apply)     shift; exec sudo "\$APP/deckopt" apply -c "\$CONFIG" "\$@" ;;
    status)    shift || true; exec "\$APP/deckopt" status -c "\$CONFIG" "\$@" ;;
    check)     exec "\$APP/deckopt" check -c "\$CONFIG" ;;
    *)         echo "usage: deckopt [check|status|toggle|update|edit|log|restart|apply|version|uninstall]" ;;
esac
EOF
chmod 755 "$home/.local/bin/deckopt"
grep -q '.local/bin' "$home/.bashrc" 2>/dev/null || echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$home/.bashrc"

# Shortcuts in the app menu and on the desktop. Add them to Steam as non-Steam games to use them from Game Mode.
shortcut() {  # id, name, icon, command
    printf '[Desktop Entry]\nType=Application\nName=%s\nIcon=%s\nExec=%s\nTerminal=false\nCategories=Utility;\n' \
        "$2" "$3" "$4" > "$home/.local/share/applications/deckopt-$1.desktop"
    if [[ -d $home/Desktop ]]; then
        cp "$home/.local/share/applications/deckopt-$1.desktop" "$home/Desktop/"
        chmod +x "$home/Desktop/deckopt-$1.desktop"
    fi
}
shortcut update "Update DeckOpt" system-software-update "konsole --hold -e $home/.local/bin/deckopt update"
rm -f "$home/.local/share/applications/deckopt-status.desktop" "$home/Desktop/deckopt-status.desktop"  # replaced by Check
shortcut check  "DeckOpt Check" utilities-system-monitor "konsole --hold -e $home/.local/bin/deckopt check"
shortcut edit   "Edit DeckOpt Profiles" document-edit "xdg-open $config_dir/profiles.toml"
shortcut toggle "DeckOpt On-Off" system-shutdown "konsole --hold -e $home/.local/bin/deckopt toggle"
USER

    say "Starting service..."
    systemctl daemon-reload
    systemctl enable deckopt.service >/dev/null 2>&1
    systemctl restart deckopt.service
    sleep 1
    systemctl is-active --quiet deckopt.service || die "Service failed to start. See: journalctl -u deckopt"

    echo
    say "DeckOpt $(cat "$APP_DIR/VERSION" 2>/dev/null || echo) is installed and running."
    sudo -u "$user" "$APP_DIR/deckopt" status -c "$config_dir/profiles.toml" 2>/dev/null || true
    cat <<EOF

  Desktop shortcuts:  DeckOpt Check, DeckOpt On-Off, Update DeckOpt, Edit DeckOpt Profiles
  Commands:           deckopt check | status | update | edit | log | uninstall
                      (open a new Konsole window first so the command is found)

  In Game Mode, leave Steam's Performance menu (TDP, GPU clock, frame limit, refresh rate)
  at the defaults so it doesn't fight DeckOpt.
EOF
}

case $CMD in
    uninstall)
        [[ $EUID -eq 0 ]] || { ensure_sudo_password; exec sudo bash "$APP_DIR/install.sh" uninstall; }
        uninstall ;;
    update|install)
        if [[ $EUID -ne 0 ]]; then
            # A source checkout (or a downloaded release) has profiles.toml next to this script; piped
            # from curl there's nothing here, so fetch everything.
            if [[ $CMD == update || ! -f $SRC/profiles.toml ]]; then
                fetch_latest
                if [[ $CMD == update && -s $SRC/VERSION && $(cat "$SRC/VERSION") == $(cat "$APP_DIR/VERSION" 2>/dev/null) ]]; then
                    say "DeckOpt is already up to date ($(cat "$SRC/VERSION"))."
                    exit 0
                fi
            fi
            ensure_sudo_password
            sudo -n true 2>/dev/null || say "Enter your password to install (nothing shows while you type)."
            exec sudo bash "$SRC/install.sh" install
        fi
        [[ -f $SRC/profiles.toml ]] || fetch_latest   # piped straight into `sudo bash`
        install_as_root ;;
    *) die "usage: install.sh [install|update|uninstall]" ;;
esac
