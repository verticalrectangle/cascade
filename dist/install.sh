#!/bin/sh
# cascade installer — one line:
#   curl -fsSL https://raw.githubusercontent.com/verticalrectangle/cascade/main/dist/install.sh/install.sh | sh
set -e

DEST="${CASCADE_DEST:-$HOME/.local/bin}"
SHARE="${CASCADE_SHARE:-$HOME/.local/share}"
APPS="$SHARE/applications"
ICONS="$SHARE/icons/hicolor/512x512/apps"
VERSION="${CASCADE_VERSION:-latest}"

if ! pkg-config --exists gtk4 2>/dev/null || ! pkg-config --exists libadwaita-1 2>/dev/null; then
    echo "cascade needs GTK4 system libraries."
    echo ""
    echo "  Arch/CachyOS:  sudo pacman -S gtk4 libadwaita"
    echo "  Ubuntu/Debian: sudo apt install libgtk-4-1 libadwaita-1-0"
    echo "  Fedora:        sudo dnf install gtk4 libadwaita"
    exit 1
fi

if [ "$VERSION" = latest ]; then
    url="https://raw.githubusercontent.com/verticalrectangle/cascade/main/dist/install.sh"
else
    url="https://github.com/verticalrectangle/cascade/releases/download/$VERSION"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading cascade ($VERSION)…" >&2
if command -v curl >/dev/null; then
    curl -fsSL "$url/cascade-linux-x86_64.tar.gz" -o "$TMP/pkg.tar.gz"
elif command -v wget >/dev/null; then
    wget -q "$url/cascade-linux-x86_64.tar.gz" -O "$TMP/pkg.tar.gz"
else
    echo "need curl or wget" >&2; exit 1
fi
tar -xzf "$TMP/pkg.tar.gz" -C "$TMP"

mkdir -p "$DEST" "$APPS" "$ICONS" "$HOME/.config/systemd/user"
install -m755 "$TMP/cascade/cascade-gtk" "$DEST/cascade-gtk"
install -m755 "$TMP/cascade/cascaded" "$DEST/cascaded"
install -m644 "$TMP/cascade/cascade.desktop" "$APPS/cascade.desktop"
install -m644 "$TMP/cascade/cascade.png" "$ICONS/cascade.png"
install -m644 "$TMP/cascade/cascaded-desktop.service" "$HOME/.config/systemd/user/cascaded.service"

echo ""
echo "cascade installed:"
echo "  $DEST/cascade-gtk    — the app (also in your app menu)"
echo "  $DEST/cascaded       — daemon binary"
echo ""
echo "run it:  cascade-gtk"
echo "make this machine spawnable from your phone: edit ~/.config/systemd/user/cascaded.service"
echo "(CASCADE_CLOUD_URL / CASCADE_MACHINE_NAME / CASCADE_MACHINE_TOKEN), then:"
echo "  systemctl --user enable --now cascaded"
