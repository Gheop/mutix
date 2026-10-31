#!/bin/sh
# Installs mutix and the Mutix training app for the current user.
set -eu
cd "$(dirname "$0")"
cargo install --path . --features gui --locked
share=${XDG_DATA_HOME:-$HOME/.local/share}
bin=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin
# GNOME hides a launcher whose program is not on the session PATH, which
# rarely includes ~/.cargo/bin: write the full path.
mkdir -p "$share/applications"
sed "s|^Exec=mutix-gtk|Exec=$bin/mutix-gtk|" assets/io.github.gheop.Mutix.desktop \
  > "$share/applications/io.github.gheop.Mutix.desktop"
update-desktop-database -q "$share/applications" 2>/dev/null || true
install -Dm644 assets/icon.svg "$share/icons/hicolor/scalable/apps/io.github.gheop.Mutix.svg"
gtk-update-icon-cache -q -t "$share/icons/hicolor" 2>/dev/null || true
echo "Installed mutix and mutix-gtk in ~/.cargo/bin; Mutix is in your applications."
