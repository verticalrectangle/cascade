#!/bin/sh
# cascade installer — one line:
#   curl -fsSL https://github.com/verticalrectangle/cascade/releases/latest/download/install.sh | sh
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
    url="https://github.com/verticalrectangle/cascade/releases/latest/download"
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

# ── cloud link: register terminal omp sessions with your account ─────────
setup_cloud() {
    if [ ! -t 0 ] || [ "${CASCADE_SETUP:-}" = "skip" ]; then
        echo "skipping cloud link (non-interactive; set up later with: cascade-gtk)"
        return 0
    fi
    printf "link your Cascade cloud account? (registers terminal omp sessions) [y/N] "
    read -r ans || return 0
    case "$ans" in y|Y|yes|YES) ;; *) echo "skipped."; return 0 ;; esac
    printf "email: "; read -r email
    printf "password: "; stty -echo; read -r password; stty echo; echo ""
    resp=$(curl -fsS -X POST "https://wickrunner.com:7701/auth/login" \
        -H "Content-Type: application/json" \
        -d "{\"email\":\"$email\",\"password\":\"$password\"}" 2>/dev/null || true)
    jwt=$(printf '%s' "$resp" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    if [ -z "$jwt" ]; then
        echo "login failed — skipping cloud link (try again: re-run install.sh)"
        return 0
    fi
    resp=$(curl -fsS -X POST "https://wickrunner.com:7701/machines/token" \
        -H "Authorization: Bearer $jwt" 2>/dev/null || true)
    tok=$(printf '%s' "$resp" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
    if [ -z "$tok" ]; then
        echo "could not mint a machine token — skipping"
        return 0
    fi
    mkdir -p "$HOME/.config/cascade"
    envfile="$HOME/.config/cascade/env"
    {
        echo "CASCADE_URL=https://wickrunner.com:7701"
        echo "CASCADE_RELAY=wss://wickrunner.com:8789"
        echo "CASCADE_TOKEN=$tok"
    } > "$envfile"
    chmod 600 "$envfile"
    for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
        if [ -f "$rc" ] && ! grep -q "config/cascade/env" "$rc"; then
            echo '[ -f "$HOME/.config/cascade/env" ] && . "$HOME/.config/cascade/env"' >> "$rc"
        fi
    done
    echo "cloud linked — new terminal omp sessions will appear in Cascade."

    # link the omp plugin if omp is present
    if command -v omp >/dev/null 2>&1; then
        omp plugin link "$SHARE/cascade/cascade-omp-plugin" >/dev/null 2>&1 && \
            echo "omp plugin linked." || echo "plugin link failed (non-fatal)"
    fi

    # offer the desktop role (make THIS machine spawnable from your phone)
    printf "make this machine spawnable from your phone/other devices? [y/N] "
    read -r ans || return 0
    case "$ans" in y|Y|yes|YES)
        unit="$HOME/.config/systemd/user/cascaded.service"
        sed -i "s|^\[Service\]|[Service]\nEnvironment=CASCADE_CLOUD_URL=https://wickrunner.com:7701\nEnvironment=CASCADE_MACHINE_NAME=$(hostname)\nEnvironment=CASCADE_MACHINE_TOKEN=$tok|" "$unit"
        systemctl --user enable --now cascaded >/dev/null 2>&1 && \
            echo "cascaded enabled — this machine is live." || \
            echo "systemd user service unavailable — enable later: systemctl --user enable --now cascaded"
        ;; esac
}

setup_cloud
echo "run it:  cascade-gtk"
echo "make this machine spawnable from your phone: edit ~/.config/systemd/user/cascaded.service"
echo "(CASCADE_CLOUD_URL / CASCADE_MACHINE_NAME / CASCADE_MACHINE_TOKEN), then:"
echo "  systemctl --user enable --now cascaded"
