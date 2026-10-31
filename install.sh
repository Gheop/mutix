#!/bin/sh
# Installs mutix and the Mutix training app for the current user.
set -eu
cd "$(dirname "$0")"
cargo install --path . --features gui --locked
share=${XDG_DATA_HOME:-$HOME/.local/share}
install -Dm644 assets/io.github.gheop.Mutix.desktop "$share/applications/io.github.gheop.Mutix.desktop"
install -Dm644 assets/icon.svg "$share/icons/hicolor/scalable/apps/io.github.gheop.Mutix.svg"
gtk-update-icon-cache -q -t "$share/icons/hicolor" 2>/dev/null || true
echo "Installed mutix and mutix-gtk in ~/.cargo/bin; Mutix is in your applications."
