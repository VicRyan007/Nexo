#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-1.0.0}"
OUT_DIR="${2:-dist}"
ARCH="amd64"
VERSION="${VERSION#v}"

if [[ ! "$VERSION" =~ ^[0-9][0-9A-Za-z.+~-]*$ ]]; then
  echo "Error: invalid Debian/package version: ${VERSION}" >&2
  exit 1
fi

TARGET_ROOT="${CARGO_TARGET_DIR:-target}"

echo "==> Building Nexo release binary for Linux x86_64..."
cargo build --release -p nexo-app

mkdir -p "$OUT_DIR"
BINARY="${TARGET_ROOT}/release/nexo"
ICON_SOURCE="crates/nexo-app/ui/assets/nexo.png"

if [[ ! -f "$BINARY" ]]; then
    echo "Error: Binary not found at $BINARY" >&2
    exit 1
fi
if [[ ! -f "$ICON_SOURCE" ]]; then
    echo "Error: Icon not found at $ICON_SOURCE" >&2
    exit 1
fi

# 1. Tarball
TAR_NAME="nexo-${VERSION}-linux-x86_64.tar.gz"
echo "==> Creating Linux tarball: ${OUT_DIR}/${TAR_NAME}..."
STAGE_DIR=$(mktemp -d)
cp "$BINARY" "${STAGE_DIR}/nexo"
cp README.md "${STAGE_DIR}/README.md"
cp LICENSE "${STAGE_DIR}/LICENSE"
cp "$ICON_SOURCE" "${STAGE_DIR}/nexo.png"
tar -czf "${OUT_DIR}/${TAR_NAME}" -C "${STAGE_DIR}" nexo README.md LICENSE nexo.png
rm -rf "${STAGE_DIR}"

# 2. Debian package (.deb)
DEB_STAGE=$(mktemp -d)
trap 'rm -rf "$DEB_STAGE"' EXIT

PKG_DIR="${DEB_STAGE}/nexo_${VERSION}_${ARCH}"
mkdir -p "${PKG_DIR}/DEBIAN"
mkdir -p "${PKG_DIR}/usr/bin"
mkdir -p "${PKG_DIR}/usr/share/applications"
mkdir -p "${PKG_DIR}/usr/share/icons/hicolor/256x256/apps"
mkdir -p "${PKG_DIR}/usr/share/doc/nexo"

cp "$BINARY" "${PKG_DIR}/usr/bin/nexo"
chmod 755 "${PKG_DIR}/usr/bin/nexo"
cp packaging/linux/nexo.desktop "${PKG_DIR}/usr/share/applications/nexo.desktop"
cp "$ICON_SOURCE" "${PKG_DIR}/usr/share/icons/hicolor/256x256/apps/nexo.png"
chmod 644 "${PKG_DIR}/usr/share/icons/hicolor/256x256/apps/nexo.png"
cp README.md "${PKG_DIR}/usr/share/doc/nexo/README.md"
cp LICENSE "${PKG_DIR}/usr/share/doc/nexo/copyright"

cat <<EOF > "${PKG_DIR}/DEBIAN/control"
Package: nexo
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Depends: libasound2 | libasound2t64, libfontconfig1, libpipewire-0.3-0 | libpipewire-0.3-0t64
Maintainer: Nexo Team <contact@nexo.local>
Description: P2P Private Offline-First Desktop Collaboration
 Nexo provides peer-to-peer communities, persistent offline-first messaging,
 encrypted voice, video, and screen sharing across local networks without central servers.
EOF

dpkg-deb --build "${PKG_DIR}" "${OUT_DIR}/nexo_${VERSION}_${ARCH}.deb"
echo "==> Debian package created successfully: ${OUT_DIR}/nexo_${VERSION}_${ARCH}.deb"
