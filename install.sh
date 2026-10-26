#!/bin/bash
# DeckOpt installer / updater for SteamOS. Paste this into Konsole (Desktop Mode):
#
#   curl -fsSL https://raw.githubusercontent.com/NoahJ456-007/DeckOpt/main/install.sh | bash
#
# After that:  deckopt update   |   deckopt uninstall   (or use the desktop shortcuts)
set -euo pipefail

REPO=NoahJ456-007/DeckOpt
APP_DIR=/var/lib/deckopt          # /var and /etc survive SteamOS updates
UNIT=/etc/systemd/system/deckopt.service

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m!!\033[0m %s\n' "$*" >&2; exit 1; }

# Where this script's files are. Empty-ish when piped from curl, so we download them.
SRC=$(cd "$(dirname "${BASH_SOURCE[0]:-.}")" 2>/dev/null && pwd || pwd)
CMD=${1:-install}

fetch_latest() {
    local tmp
    tmp=$(mktemp -d)
    say "Downloading the latest DeckOpt..."
    curl -fsSL "https://github.com/$REPO/archive/refs/heads/main.tar.gz" | tar xz -C "$tmp" --strip-components=1 \
        || die "Download failed. Check your internet connection."
    curl -fsSL "https://api.github.com/repos/$REPO/commits/main" 2>/dev/null \
        | grep -m1 '"sha"' | cut -d'"' -f4 | cut -c1-7 > "$tmp/VERSION" || true
    SRC=$tmp
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

install_as_root() {
    local user=${SUDO_USER:-deck} home config_dir
    home=$(getent passwd "$user" | cut -d: -f6)
    config_dir=$home/.config/deckopt

    say "Installing service..."
    install -d -m 755 "$APP_DIR"
    install -m 755 "$SRC/deckopt.py" "$SRC/install.sh" "$APP_DIR/"
    install -m 644 "$SRC/profiles.toml" "$SRC/deckopt.service" "$APP_DIR/"
    [[ -s $SRC/VERSION ]] && install -m 644 "$SRC/VERSION" "$APP_DIR/VERSION"
    sed "s|@CONFIG@|$config_dir/profiles.toml|; s|@APP_DIR@|$APP_DIR|" "$SRC/deckopt.service" > "$UNIT"

    # Profiles belong to the user. Never overwrite theirs; keep the latest examples beside it.
    sudo -u "$user" bash -s "$SRC" "$config_dir" "$home" <<'USER'
set -e
src=$1 config_dir=$2 home=$3
mkdir -p "$config_dir" "$home/.local/bin" "$home/.local/share/applications"
[[ -f $config_dir/profiles.toml ]] || cp "$src/profiles.toml" "$config_dir/profiles.toml"
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
    version)   cat "\$APP/VERSION" 2>/dev/null || echo unknown ;;
    apply)     shift; exec sudo python3 "\$APP/deckopt.py" apply -c "\$CONFIG" "\$@" ;;
    status)    shift || true; exec python3 "\$APP/deckopt.py" status -c "\$CONFIG" "\$@" ;;
    check)     exec python3 "\$APP/deckopt.py" check -c "\$CONFIG" ;;
    *)         echo "usage: deckopt [check|status|update|edit|log|restart|apply|version|uninstall]" ;;
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
USER

    say "Starting service..."
    systemctl daemon-reload
    systemctl enable deckopt.service >/dev/null 2>&1
    systemctl restart deckopt.service
    sleep 1
    systemctl is-active --quiet deckopt.service || die "Service failed to start. See: journalctl -u deckopt"

    echo
    say "DeckOpt $(cat "$APP_DIR/VERSION" 2>/dev/null || echo) is installed and running."
    sudo -u "$user" python3 "$APP_DIR/deckopt.py" status -c "$config_dir/profiles.toml" 2>/dev/null || true
    cat <<EOF

  Desktop shortcuts:  DeckOpt Check, Update DeckOpt, Edit DeckOpt Profiles
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
            if [[ $CMD == update || ! -f $SRC/deckopt.py ]]; then
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
        [[ -f $SRC/deckopt.py ]] || fetch_latest   # piped straight into `sudo bash`
        install_as_root ;;
    *) die "usage: install.sh [install|update|uninstall]" ;;
esac
