#!/usr/bin/env sh
# Installs duet + duetctl, the launcher entry and the icon for the current
# user (no root): binaries in ~/.local/bin, the rest under ~/.local/share.
# Run from the repository root (builds from source) or from an unpacked
# release tarball (uses the prebuilt binaries next to this script).
# `./install.sh --uninstall` removes exactly what this installs.
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
DATA="${XDG_DATA_HOME:-$PREFIX/share}"
HERE="$(cd "$(dirname "$0")" && pwd)"
APP_ID=dev.fernandoa.duet

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$PREFIX/bin/duet" "$PREFIX/bin/duetctl" \
        "$DATA/applications/$APP_ID.desktop"
    rm -f "$DATA"/icons/hicolor/*/apps/"$APP_ID".png
    echo "Removed duet. Your workspaces in $DATA/duet were left alone."
    exit 0
fi

mkdir -p "$PREFIX/bin"
if [ -f "$HERE/Cargo.toml" ]; then
    cargo install --locked --path "$HERE" --root "$PREFIX"
else
    install -m 755 "$HERE/duet" "$HERE/duetctl" "$PREFIX/bin/"
fi

# Absolute Exec path: a graphical session's PATH often lacks ~/.local/bin.
mkdir -p "$DATA/applications"
sed "s|^Exec=duet$|Exec=$PREFIX/bin/duet|" "$HERE/data/$APP_ID.desktop" \
    > "$DATA/applications/$APP_ID.desktop"
chmod 644 "$DATA/applications/$APP_ID.desktop"
for icon in "$HERE"/data/icons/hicolor/*/apps/"$APP_ID".png; do
    size="$(basename "$(dirname "$(dirname "$icon")")")"
    install -Dm 644 "$icon" "$DATA/icons/hicolor/$size/apps/$APP_ID.png"
done

# Refresh caches when the tools exist (launchers usually notice anyway).
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$DATA/applications" >/dev/null 2>&1 || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -q -t -f "$DATA/icons/hicolor" >/dev/null 2>&1 || true

case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "Note: add $PREFIX/bin to your PATH to run duet/duetctl from a terminal (the launcher works either way)." ;;
esac
echo "Installed Duet — look for it in your app launcher."
