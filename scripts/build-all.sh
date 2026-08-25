#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-1.0.0}"
OUT_DIR="${2:-dist}"
TOOLCHAIN="${NEXO_RUST_TOOLCHAIN:-}"

if [[ -n "$TOOLCHAIN" && "$TOOLCHAIN" != +* ]]; then
  TOOLCHAIN="+$TOOLCHAIN"
fi

cargo_args=()
if [[ -n "$TOOLCHAIN" ]]; then
  cargo_args+=("$TOOLCHAIN")
fi

echo "======================================================"
echo "   Nexo v${VERSION} - Linux Build and Test Pipeline   "
echo "======================================================"

echo -e "\n[1/4] Verifying code formatting (cargo fmt)..."
cargo "${cargo_args[@]}" fmt --all --check

echo -e "\n[2/4] Running strict static analysis (cargo clippy)..."
cargo "${cargo_args[@]}" clippy --workspace --all-targets -- -D warnings

echo -e "\n[3/4] Running full test suite..."
run_test() {
  cargo "${cargo_args[@]}" test "$@" -- --test-threads=1
}

# Cargo can run separate integration-test binaries concurrently even when
# each binary receives --test-threads=1. These scenarios use shared local
# discovery resources, so keep the binaries themselves serialized.
run_test --workspace --lib --bins
run_test -p nexo-app --test two_instances
run_test -p nexo-app --test three_instances
run_test -p nexo-net --test file_transfer_loopback
run_test -p nexo-net --test sync_loopback
run_test -p nexo-net --test voice_loopback
run_test -p nexo-media --test video_loopback
run_test -p nexo-video --test camera_capture
run_test -p nexo-video --test screen_capture

echo -e "\n[4/4] Packaging Linux distribution (.deb and .tar.gz)..."
chmod +x scripts/package-linux.sh
./scripts/package-linux.sh "$VERSION" "$OUT_DIR"

echo -e "\n======================================================"
echo " [SUCCESS] Linux Golden Master Build Complete!"
echo " Artifacts generated in ${OUT_DIR}/"
echo "======================================================"
