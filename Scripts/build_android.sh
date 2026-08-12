#!/usr/bin/env bash
# build_android.sh
#
# Build Android shared libraries (.so) for the Rust `monerowalletcore` crate using the Android NDK toolchain.
#
# Output layout (default):
#   MoneroWalletCoreFFI/Artifacts/android/<abi>/libmonerowalletcore.so
#
# Requirements:
# - Rust toolchain + cargo
# - rustup (recommended) to install android targets
# - Android NDK installed
#
# Environment variables:
#   ANDROID_NDK_HOME or ANDROID_NDK_ROOT  : path to the NDK (required unless auto-detect succeeds)
#   ANDROID_API                          : Android API level to target (default: 26)
#   PROFILE                              : release|debug (default: release)
#   CARGO_FEATURES                        : optional cargo features (space-separated), e.g. "compile-time-generators"
#   ABIS                                 : optional comma-separated ABI list (default: arm64-v8a,x86_64)
#   OUT_DIR                              : output root (default: MoneroWalletCoreFFI/Artifacts/android)
#
# Optional post-build install step:
#   INSTALL_TO_NEXAWAL_ANDROID            : if "1", copy built .so into nexawal-android *walletcore module* jniLibs
#                                          (this avoids duplicate packaging vs app/src/main/jniLibs)
#   NEXAWAL_ANDROID_DIR                   : override path to nexawal-android (default: repo-root/../nexawal-android)
#   NEXAWAL_ANDROID_JNILIBS_DIR           : override jniLibs dir
#                                          default: $NEXAWAL_ANDROID_DIR/walletcore/src/main/jniLibs
#                                          legacy app module: $NEXAWAL_ANDROID_DIR/app/src/main/jniLibs
#
# Examples:
#   PROFILE=release CARGO_FEATURES="compile-time-generators" ./Scripts/build_android.sh
#   ANDROID_NDK_HOME=~/Library/Android/sdk/ndk/26.3.11579264 ANDROID_API=26 ./Scripts/build_android.sh
#   ABIS=arm64-v8a,x86_64,armeabi-v7a ./Scripts/build_android.sh
#
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

# Resolve repo paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CRATE_DIR="${REPO_ROOT}/monero-oxide-output"
ARTIFACTS_DIR="${REPO_ROOT}/Artifacts"

# Config
ANDROID_API="${ANDROID_API:-26}"
PROFILE="${PROFILE:-release}"
CARGO_FEATURES="${CARGO_FEATURES:-}"
ABIS="${ABIS:-arm64-v8a,x86_64}"
OUT_DIR="${OUT_DIR:-${ARTIFACTS_DIR}/android}"

# Optional: install artifacts into nexawal-android after building
INSTALL_TO_NEXAWAL_ANDROID="${INSTALL_TO_NEXAWAL_ANDROID:-0}"
NEXAWAL_ANDROID_DIR="${NEXAWAL_ANDROID_DIR:-${REPO_ROOT}/../nexawal-android}"
# Default to the :walletcore module jniLibs to avoid duplicate packaging with the app module.
NEXAWAL_ANDROID_JNILIBS_DIR="${NEXAWAL_ANDROID_JNILIBS_DIR:-${NEXAWAL_ANDROID_DIR}/walletcore/src/main/jniLibs}"

# Tooling
CARGO_BIN="$(command -v cargo || true)"
RUSTUP_BIN="$(command -v rustup || true)"

[[ -n "${CARGO_BIN}" ]] || die "cargo not found (install Rust toolchain: https://rustup.rs/)"
[[ -d "${CRATE_DIR}" ]] || die "crate dir not found: ${CRATE_DIR}"
[[ -f "${CRATE_DIR}/Cargo.toml" ]] || die "Cargo.toml not found: ${CRATE_DIR}/Cargo.toml"

# Profile flag
CARGO_PROFILE_FLAG=""
if [[ "${PROFILE}" == "release" ]]; then
  CARGO_PROFILE_FLAG="--release"
elif [[ "${PROFILE}" == "debug" ]]; then
  CARGO_PROFILE_FLAG=""
else
  die "PROFILE must be 'debug' or 'release' (got '${PROFILE}')"
fi

# Features flag
CARGO_FEATURES_FLAG=""
if [[ -n "${CARGO_FEATURES}" ]]; then
  # shellcheck disable=SC2206
  CARGO_FEATURES_ARR=( ${CARGO_FEATURES} )
  CARGO_FEATURES_FLAG="--features $(IFS=,; echo "${CARGO_FEATURES_ARR[*]}")"
fi

# NDK detection
NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "${NDK_HOME}" ]]; then
  # Common macOS locations (best-effort). You can override via ANDROID_NDK_HOME.
  if [[ -d "${HOME}/Library/Android/sdk/ndk" ]]; then
    # Pick the newest version directory
    NDK_HOME="$(ls -1d "${HOME}/Library/Android/sdk/ndk/"* 2>/dev/null | sort -V | tail -n 1 || true)"
  fi
fi
[[ -n "${NDK_HOME}" && -d "${NDK_HOME}" ]] || die "Android NDK not found. Set ANDROID_NDK_HOME or ANDROID_NDK_ROOT."

HOST_TAG=""
uname_s="$(uname -s | tr '[:upper:]' '[:lower:]')"
uname_m="$(uname -m)"
case "${uname_s}" in
  darwin) HOST_TAG="darwin-x86_64" ;;
  linux)  HOST_TAG="linux-x86_64" ;;
  *)
    die "unsupported host OS: ${uname_s}"
    ;;
esac
# NDK on Apple Silicon uses the same prebuilt folder name currently (darwin-x86_64), but some installs provide darwin-arm64.
# Prefer darwin-arm64 if present.
if [[ "${uname_s}" == "darwin" && "${uname_m}" == "arm64" && -d "${NDK_HOME}/toolchains/llvm/prebuilt/darwin-arm64" ]]; then
  HOST_TAG="darwin-arm64"
fi

LLVM_PREBUILT="${NDK_HOME}/toolchains/llvm/prebuilt/${HOST_TAG}"
[[ -d "${LLVM_PREBUILT}" ]] || die "NDK llvm toolchain not found at: ${LLVM_PREBUILT}"

export AR="${LLVM_PREBUILT}/bin/llvm-ar"
export RANLIB="${LLVM_PREBUILT}/bin/llvm-ranlib"
export STRIP="${LLVM_PREBUILT}/bin/llvm-strip"

# ABI -> Rust target + clang triple mapping
rust_target_for_abi() {
  case "$1" in
    arm64-v8a)   echo "aarch64-linux-android" ;;
    x86_64)      echo "x86_64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86)         echo "i686-linux-android" ;;
    *) die "unsupported ABI: $1" ;;
  esac
}

clang_triple_for_abi() {
  case "$1" in
    arm64-v8a)   echo "aarch64-linux-android" ;;
    x86_64)      echo "x86_64-linux-android" ;;
    armeabi-v7a) echo "armv7a-linux-androideabi" ;;
    x86)         echo "i686-linux-android" ;;
    *) die "unsupported ABI: $1" ;;
  esac
}

# Install rust targets if rustup is available
ensure_rust_target() {
  local target="$1"
  if [[ -n "${RUSTUP_BIN}" ]]; then
    echo "• Ensuring Rust target '${target}' is installed"
    "${RUSTUP_BIN}" target add "${target}" >/dev/null 2>&1 || true
  else
    echo "warning: rustup not found; assuming target '${target}' is already installed"
  fi
}

build_one() {
  local abi="$1"
  local rust_target
  rust_target="$(rust_target_for_abi "${abi}")"

  local clang_triple
  clang_triple="$(clang_triple_for_abi "${abi}")"

  local cc="${LLVM_PREBUILT}/bin/${clang_triple}${ANDROID_API}-clang"
  local cxx="${LLVM_PREBUILT}/bin/${clang_triple}${ANDROID_API}-clang++"

  [[ -x "${cc}" ]] || die "clang not found/executable: ${cc}"

  echo "== Building for ABI=${abi} (target=${rust_target}, api=${ANDROID_API}, profile=${PROFILE}) =="

  ensure_rust_target "${rust_target}"

  # Tell cargo which linker to use for this target. This is the most reliable approach for NDK builds.
  export "CARGO_TARGET_$(echo "${rust_target}" | tr '[:lower:]-' '[:upper:]_')_LINKER"="${cc}"

  # Android 15+ 16 KB page-size compatibility (ELF PT_LOAD align >= 16384).
  # https://developer.android.com/guide/practices/page-sizes
  # Keep any caller-provided RUSTFLAGS; always append the page-size link args.
  local page_flags="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384"
  if [[ -n "${RUSTFLAGS:-}" ]]; then
    export RUSTFLAGS="${RUSTFLAGS} ${page_flags}"
  else
    export RUSTFLAGS="${page_flags}"
  fi

  # Some crates/build scripts respect CC/CXX as well.
  export CC="${cc}"
  export CXX="${cxx}"

  # Keep artifacts in-tree (avoid sandbox/redirected target dirs copying stale .so files).
  export CARGO_TARGET_DIR="${CRATE_DIR}/target"

  (cd "${CRATE_DIR}" && "${CARGO_BIN}" build ${CARGO_PROFILE_FLAG} ${CARGO_FEATURES_FLAG} --target "${rust_target}")

  local built_so="${CARGO_TARGET_DIR}/${rust_target}/${PROFILE}/libmonerowalletcore.so"
  [[ -f "${built_so}" ]] || die "expected .so not found: ${built_so}"

  # Verify 16 KB ELF LOAD alignment before packaging.
  if [[ -x "${LLVM_PREBUILT}/bin/llvm-readelf" ]]; then
    if ! "${LLVM_PREBUILT}/bin/llvm-readelf" -l "${built_so}" | awk '/LOAD/ {print $NF}' | grep -q '0x4000'; then
      die "libmonerowalletcore.so for ${abi} is not 16KB-aligned (expected PT_LOAD Align 0x4000). RUSTFLAGS='${RUSTFLAGS}'"
    fi
  fi

  local out_abi_dir="${OUT_DIR}/${abi}"
  mkdir -p "${out_abi_dir}"
  cp -f "${built_so}" "${out_abi_dir}/libmonerowalletcore.so"

  # Strip in release to reduce size (best-effort)
  if [[ "${PROFILE}" == "release" ]]; then
    "${STRIP}" --strip-unneeded "${out_abi_dir}/libmonerowalletcore.so" 2>/dev/null || true
  fi

  # Re-check after strip (should still be 0x4000).
  if [[ -x "${LLVM_PREBUILT}/bin/llvm-readelf" ]]; then
    if ! "${LLVM_PREBUILT}/bin/llvm-readelf" -l "${out_abi_dir}/libmonerowalletcore.so" | awk '/LOAD/ {print $NF}' | grep -q '0x4000'; then
      die "stripped libmonerowalletcore.so for ${abi} lost 16KB alignment"
    fi
  fi

  echo "• Wrote: ${out_abi_dir}/libmonerowalletcore.so (16KB-aligned)"

  # Optional install into nexawal-android's jniLibs
  if [[ "${INSTALL_TO_NEXAWAL_ANDROID}" == "1" ]]; then
    local dest_dir="${NEXAWAL_ANDROID_JNILIBS_DIR}/${abi}"
    mkdir -p "${dest_dir}"
    cp -f "${out_abi_dir}/libmonerowalletcore.so" "${dest_dir}/libmonerowalletcore.so"
    echo "• Installed: ${dest_dir}/libmonerowalletcore.so"
  fi
}

echo "== MoneroWalletCore Android .so build =="
echo "  - Repo root: ${REPO_ROOT}"
echo "  - Crate dir: ${CRATE_DIR}"
echo "  - NDK:       ${NDK_HOME}"
echo "  - Toolchain: ${LLVM_PREBUILT}"
echo "  - API:       ${ANDROID_API}"
echo "  - Profile:   ${PROFILE}"
echo "  - Features:  ${CARGO_FEATURES:-<none>}"
echo "  - ABIs:      ${ABIS}"
echo "  - OUT_DIR:   ${OUT_DIR}"
echo

# Build for each ABI
IFS=',' read -r -a ABI_LIST <<< "${ABIS}"
for abi in "${ABI_LIST[@]}"; do
  build_one "${abi}"
done

echo
echo "== Done =="
echo "Artifacts are in: ${OUT_DIR}"
echo

if [[ "${INSTALL_TO_NEXAWAL_ANDROID}" == "1" ]]; then
  echo "Installed into nexawal-android :walletcore jniLibs:"
  echo "  ${NEXAWAL_ANDROID_JNILIBS_DIR}/arm64-v8a/libmonerowalletcore.so"
  echo "  ${NEXAWAL_ANDROID_JNILIBS_DIR}/x86_64/libmonerowalletcore.so"
else
  echo "Next step: copy these into your Android :walletcore module's jniLibs, e.g.:"
  echo "  walletcore/src/main/jniLibs/arm64-v8a/libmonerowalletcore.so"
  echo "  walletcore/src/main/jniLibs/x86_64/libmonerowalletcore.so"
  echo
  echo "Or install automatically into nexawal-android by setting:"
  echo "  INSTALL_TO_NEXAWAL_ANDROID=1"
  echo "Optionally override paths:"
  echo "  NEXAWAL_ANDROID_DIR=/path/to/nexawal-android"
  echo "  NEXAWAL_ANDROID_JNILIBS_DIR=/path/to/nexawal-android/walletcore/src/main/jniLibs"
  echo
  echo "Note: Avoid also copying into app/src/main/jniLibs, or you'll get duplicate .so packaging warnings."
fi
