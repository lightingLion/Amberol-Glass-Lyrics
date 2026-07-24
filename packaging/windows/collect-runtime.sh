#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
APP_DIR=${APP_DIR:-"$ROOT/dist/windows/Amberol Glass Lyrics"}
TARGET=${TARGET:-x86_64-pc-windows-gnu}
MINGW_PREFIX=${MINGW_PREFIX:-/ucrt64}

rm -rf "$APP_DIR"
mkdir -p \
  "$APP_DIR/lib" \
  "$APP_DIR/share/glib-2.0/schemas" \
  "$APP_DIR/share/icons" \
  "$APP_DIR/etc/gtk-4.0"

cp "$ROOT/target/$TARGET/release/amberol.exe" \
  "$APP_DIR/amberol-glass-lyrics.exe"
cp "$ROOT/build-windows/amberol-glass-lyrics.gresource" "$APP_DIR/"
cp "$ROOT/packaging/windows/amberol-glass-lyrics.ico" "$APP_DIR/"
cp "$ROOT/README.md" "$APP_DIR/"
cp "$ROOT/LICENSES/GPL-3.0-or-later.txt" "$APP_DIR/LICENSE.txt"

# GTK and GStreamer on Windows are distributed as an application-local
# runtime. Keeping the DLLs next to the executable also makes the resulting
# MSI independent of an end-user MSYS2 installation.
cp "$MINGW_PREFIX"/bin/*.dll "$APP_DIR/"

for directory in \
  gstreamer-1.0 \
  gio/modules \
  gdk-pixbuf-2.0 \
  gtk-4.0; do
  if [[ -d "$MINGW_PREFIX/lib/$directory" ]]; then
    mkdir -p "$APP_DIR/lib/$directory"
    cp -a "$MINGW_PREFIX/lib/$directory/." "$APP_DIR/lib/$directory/"
  fi
done

for directory in \
  glib-2.0/schemas \
  icons/Adwaita \
  icons/hicolor \
  gstreamer-1.0; do
  if [[ -d "$MINGW_PREFIX/share/$directory" ]]; then
    mkdir -p "$APP_DIR/share/$directory"
    cp -a "$MINGW_PREFIX/share/$directory/." "$APP_DIR/share/$directory/"
  fi
done

cp "$ROOT/data/io.bassi.Amberol.gschema.xml" \
  "$APP_DIR/share/glib-2.0/schemas/"
glib-compile-schemas "$APP_DIR/share/glib-2.0/schemas"

cat > "$APP_DIR/etc/gtk-4.0/settings.ini" <<'EOF'
[Settings]
gtk-font-name=Segoe UI 10
gtk-decoration-layout=menu:minimize,maximize,close
EOF

# The generated loader cache contains the CI runner's absolute staging path.
# Convert it to paths relative to the install directory; the MSI shortcut sets
# that directory as the process working directory.
loader_dir="$APP_DIR/lib/gdk-pixbuf-2.0/2.10.0/loaders"
if [[ -d "$loader_dir" ]]; then
  gdk-pixbuf-query-loaders "$loader_dir"/*.dll > \
    "$APP_DIR/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
  app_prefix=$(cygpath -m "$APP_DIR")
  sed -i "s|$app_prefix/||g" \
    "$APP_DIR/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
fi

echo "$APP_DIR"
