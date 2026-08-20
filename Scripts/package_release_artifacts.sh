#!/usr/bin/env bash
# Package source-built native outputs for a WalletCore GitHub release.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
ARTIFACTS_DIR="${ARTIFACTS_DIR:-${REPO_ROOT}/.build/artifacts}"
DIST_DIR="${DIST_DIR:-${REPO_ROOT}/dist}"

die() {
  echo "error: $*" >&2
  exit 1
}

command -v zip >/dev/null 2>&1 || die "zip is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"

if [[ -d "${ARTIFACTS_DIR}/MoneroWalletCore.xcframework" ]]; then
  ditto -c -k --sequesterRsrc --keepParent \
    "${ARTIFACTS_DIR}/MoneroWalletCore.xcframework" \
    "${DIST_DIR}/MoneroWalletCore.xcframework.zip"
fi

if [[ -d "${ARTIFACTS_DIR}/android" ]]; then
  (cd "${ARTIFACTS_DIR}" && zip -q -r \
    "${DIST_DIR}/MoneroWalletCore-android.zip" android)
fi

shopt -s nullglob
release_files=("${DIST_DIR}"/*.zip)
(( ${#release_files[@]} > 0 )) || die "no native artifacts found under ${ARTIFACTS_DIR}"

{
  echo "MoneroWalletCore native artifact build"
  echo "source_commit=${SOURCE_COMMIT:-$(git -C "${REPO_ROOT}" rev-parse HEAD)}"
  echo "rustc=${RUSTC_VERSION:-$(rustc --version 2>/dev/null || echo unknown)}"
  echo "xcode=${XCODE_VERSION:-unknown}"
  echo "android_ndk=${ANDROID_NDK_VERSION:-unknown}"
  echo "android_api=${ANDROID_API:-26}"
  echo "cargo_features=${CARGO_FEATURES:-compile-time-generators}"
} > "${DIST_DIR}/BUILD-MANIFEST.txt"

(cd "${DIST_DIR}" && shasum -a 256 *.zip > SHA256SUMS.txt)
echo "Packaged native artifacts in ${DIST_DIR}"
ls -lh "${DIST_DIR}"
