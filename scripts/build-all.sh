#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-1.0.0}"
OUT_DIR="${2:-dist}"

echo "======================================================"
echo "   Nexo v${VERSION} - Linux Build and Test Pipeline   "
echo "======================================================"

echo -e "\n[1/4] Verifying code formatting (cargo fmt)..."
cargo fmt --all --check

echo -e "\n[2/4] Running strict static analysis (cargo clippy)..."
cargo clippy --workspace --all-targets -- -D warnings

echo -e "\n[3/4] Running full test suite..."
cargo test --workspace

echo -e "\n[4/4] Packaging Linux distribution (.deb and .tar.gz)..."
chmod +x scripts/package-linux.sh
./scripts/package-linux.sh "$VERSION" "$OUT_DIR"

echo -e "\n======================================================"
echo " [SUCCESS] Linux Golden Master Build Complete!"
echo " Artifacts generated in ${OUT_DIR}/"
echo "======================================================"
