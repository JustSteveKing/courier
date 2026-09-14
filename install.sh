#!/usr/bin/env bash
# Builds Courier and installs it for the current user (or into $PREFIX).
#
#   ./install.sh               build and install to ~/.local
#   ./install.sh --uninstall   remove what this script installed
#   PREFIX=/opt/courier ./install.sh
set -euo pipefail

APP_ID=dev.steve.courier
PREFIX=${PREFIX:-$HOME/.local}
ROOT=$(cd "$(dirname "$0")" && pwd)

BIN=$PREFIX/bin/courier
DESKTOP=$PREFIX/share/applications/$APP_ID.desktop
ICON=$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg

refresh_caches() {
  command -v update-desktop-database >/dev/null && update-desktop-database -q "$PREFIX/share/applications" || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" || true
}

if [[ ${1:-} == "--uninstall" ]]; then
  rm -f "$BIN" "$DESKTOP" "$ICON"
  refresh_caches
  echo "Removed Courier from $PREFIX"
  echo "Your settings, state and cache are kept in ~/.config/courier, ~/.local/state/courier and ~/.cache/courier."
  exit 0
fi

cargo build --release --manifest-path "$ROOT/Cargo.toml"

install -Dm755 "$ROOT/target/release/courier" "$BIN"
install -Dm644 "$ROOT/assets/$APP_ID.desktop" "$DESKTOP"
install -Dm644 "$ROOT/assets/$APP_ID.svg" "$ICON"
# Point the launcher at the installed binary, so it works even if $PREFIX/bin isn't on PATH.
sed -i "s|^Exec=courier|Exec=$BIN|" "$DESKTOP"
refresh_caches

echo "Installed Courier to $BIN"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "Note: $PREFIX/bin is not on your PATH; the app menu entry still works." ;;
esac
