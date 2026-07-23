#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BUILD_DIR=${BUILD_DIR:-"$ROOT/build-deb"}
DIST_DIR=${DIST_DIR:-"$ROOT/dist"}
VERSION=${VERSION:-"$(sed -n "s/.*version: '\([^']*\)'.*/\1/p" "$ROOT/meson.build" | head -n1)-1"}
ARCH=${ARCH:-"$(dpkg --print-architecture)"}
PACKAGE=amberol-glass-lyrics
STAGE=$(mktemp -d)
META=$(mktemp -d)
trap 'python3 - "$STAGE" "$META" <<"PY"
from pathlib import Path
import shutil, sys
for value in sys.argv[1:]:
    path = Path(value)
    if path.exists():
        shutil.rmtree(path)
PY' EXIT

if [[ -f "$BUILD_DIR/meson-private/coredata.dat" ]]; then
    meson setup --reconfigure "$BUILD_DIR" "$ROOT" \
        --prefix=/usr --buildtype=release -Dprofile=default
else
    meson setup "$BUILD_DIR" "$ROOT" \
        --prefix=/usr --buildtype=release -Dprofile=default
fi
meson compile -C "$BUILD_DIR"
DESTDIR="$STAGE" meson install --no-rebuild -C "$BUILD_DIR"

BINARY="$STAGE/usr/bin/amberol-glass-lyrics"
test -x "$BINARY"
mkdir -p "$STAGE/DEBIAN" "$META/debian" "$DIST_DIR"
cat > "$META/debian/control" <<CONTROL
Source: $PACKAGE
Section: sound
Priority: optional
Maintainer: Amberol Glass Lyrics Contributors <noreply@example.invalid>
Standards-Version: 4.6.2

Package: $PACKAGE
Architecture: $ARCH
Description: Amberol-based player with Gray-Scott generated lyrics
CONTROL

SHLIB_DEPS=$(
    cd "$META"
    dpkg-shlibdeps -O -e"$BINARY" 2>/dev/null | sed -n 's/^shlibs:Depends=//p'
)
EXTRA_DEPS='gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, libglib2.0-bin, desktop-file-utils'
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: $PACKAGE
Version: $VERSION
Section: sound
Priority: optional
Architecture: $ARCH
Maintainer: Amberol Glass Lyrics Contributors <noreply@example.invalid>
Depends: $SHLIB_DEPS, $EXTRA_DEPS
Homepage: https://github.com/lightingLion/Amberol-Glass-Lyrics
Description: Amberol-based music player with Gray-Scott generated lyrics
 Amberol Glass Lyrics keeps Amberol's local playback experience and adds an
 album-colored reaction-diffusion canvas where synchronized lyrics form and
 dissolve as chemical cavities.
CONTROL

cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
command -v glib-compile-schemas >/dev/null && glib-compile-schemas /usr/share/glib-2.0/schemas || true
command -v gtk4-update-icon-cache >/dev/null && gtk4-update-icon-cache -qtf /usr/share/icons/hicolor || true
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
POSTINST
cat > "$STAGE/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
command -v glib-compile-schemas >/dev/null && glib-compile-schemas /usr/share/glib-2.0/schemas || true
command -v gtk4-update-icon-cache >/dev/null && gtk4-update-icon-cache -qtf /usr/share/icons/hicolor || true
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
POSTRM
chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/postrm"
find "$STAGE" -type d -exec chmod 0755 {} +

OUTPUT="$DIST_DIR/${PACKAGE}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUTPUT"
dpkg-deb --info "$OUTPUT"
echo "$OUTPUT"
