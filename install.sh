#!/bin/bash
# Install DeckOpt as a systemd service on SteamOS.
#   sudo ./install.sh            install / update
#   sudo ./install.sh --uninstall
set -euo pipefail

APP_DIR=/var/lib/deckopt          # /var and /etc survive SteamOS updates
UNIT=/etc/systemd/system/deckopt.service
BIN=/usr/local/bin/deckopt
DECK_USER=${SUDO_USER:-deck}
CONFIG_DIR=/home/$DECK_USER/.config/deckopt

if [[ $EUID -ne 0 ]]; then
    echo "Run with sudo: sudo $0 $*" >&2
    exit 1
fi

if [[ ${1:-} == --uninstall ]]; then
    systemctl disable --now deckopt.service 2>/dev/null || true
    rm -rf "$APP_DIR" "$UNIT"
    rm -f "$BIN" 2>/dev/null || true
    systemctl daemon-reload
    echo "DeckOpt removed (your profiles in $CONFIG_DIR were kept)."
    exit 0
fi

SRC=$(cd "$(dirname "$0")" && pwd)

install -d -m 755 "$APP_DIR"
install -m 755 "$SRC/deckopt.py" "$APP_DIR/deckopt.py"
install -m 644 "$SRC/profiles.toml" "$APP_DIR/profiles.toml"

if [[ ! -f $CONFIG_DIR/profiles.toml ]]; then
    install -d -o "$DECK_USER" -g "$DECK_USER" "$CONFIG_DIR"
    install -m 644 -o "$DECK_USER" -g "$DECK_USER" "$SRC/profiles.toml" "$CONFIG_DIR/profiles.toml"
fi

sed "s|@CONFIG@|$CONFIG_DIR/profiles.toml|; s|@APP_DIR@|$APP_DIR|" "$SRC/deckopt.service" > "$UNIT"

# /usr is read-only on SteamOS; the `deckopt` shortcut is a nice-to-have.
cat > "$BIN" 2>/dev/null <<EOF || echo "(skipping $BIN, read-only filesystem; use $APP_DIR/deckopt.py)"
#!/bin/sh
exec python3 $APP_DIR/deckopt.py --config $CONFIG_DIR/profiles.toml "\$@"
EOF
chmod 755 "$BIN" 2>/dev/null || true

systemctl daemon-reload
systemctl enable --now deckopt.service
systemctl restart deckopt.service

echo
echo "DeckOpt installed and running."
echo "  Edit profiles:  $CONFIG_DIR/profiles.toml   (reloaded automatically)"
echo "  Live log:       journalctl -u deckopt -f"
echo "  Current state:  python3 $APP_DIR/deckopt.py status -c $CONFIG_DIR/profiles.toml"
