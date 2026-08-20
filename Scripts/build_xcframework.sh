#!/usr/bin/env bash
# build_xcframework.sh
# Build Apple static libraries for the Rust monerowalletcore crate and package them into an .xcframework

set -euo pipefail

# Default Apple deployment targets (can be overridden by env)
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"
export IPHONESIMULATOR_DEPLOYMENT_TARGET="${IPHONESIMULATOR_DEPLOYMENT_TARGET:-16.0}"

# -----------------------------
# Configuration and environment
# -----------------------------
#
# NOTE:
# We no longer support "fork mode" via local path patches.
# The walletcore crate is expected to pin monero-oxide dependencies by git URL + commit (rev),
# making builds reproducible on CI and other machines without requiring a sibling checkout.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CRATE_DIR="${REPO_ROOT}/monero-oxide-output"
OUT_DIR="${OUT_DIR:-${REPO_ROOT}/.build/artifacts}"
FRAMEWORK_NAME="MoneroWalletCore"
XCFRAMEWORK_PATH="${OUT_DIR}/${FRAMEWORK_NAME}.xcframework"

# Keep artifacts in-tree (sandbox CARGO_TARGET_DIR otherwise packages stale libs).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${CRATE_DIR}/target}"

# Build profile: release (default) or debug
PROFILE="${PROFILE:-release}" # set PROFILE=debug to build debug libs

# Cargo flag: only --release is valid. Debug is the default (no flag).
CARGO_PROFILE_FLAG=""
if [[ "${PROFILE}" == "release" ]]; then
  CARGO_PROFILE_FLAG="--release"
elif [[ "${PROFILE}" == "debug" ]]; then
  CARGO_PROFILE_FLAG=""
else
  die "PROFILE must be 'debug' or 'release' (got '${PROFILE}')"
fi

# Optional Cargo features: set CARGO_FEATURES="feat1 feat2" (space-separated) to enable.
# Example:
#   PROFILE=release CARGO_FEATURES="scanner-microprof" Scripts/build_xcframework.sh
CARGO_FEATURES="${CARGO_FEATURES:-}"
CARGO_FEATURES_FLAG=""
if [[ -n "${CARGO_FEATURES}" ]]; then
  # shellcheck disable=SC2206
  CARGO_FEATURES_ARR=( ${CARGO_FEATURES} )
  CARGO_FEATURES_FLAG="--features $(IFS=,; echo "${CARGO_FEATURES_ARR[*]}")"
fi

# Tooling detection
XCODEBUILD_BIN="$(command -v xcodebuild || true)"
RUSTUP_BIN="$(command -v rustup || true)"
CARGO_BIN="$(command -v cargo || true)"

# Header/modulemap
HEADER_SRC="${REPO_ROOT}/CLibMoneroWalletCore/monerowalletcore.h"

# Targets to build (adjust as needed)
APPLE_TARGETS=(
  "aarch64-apple-ios"        # iOS device (arm64)
  "aarch64-apple-ios-sim"    # iOS simulator (arm64)
  "aarch64-apple-darwin"     # macOS (arm64)
  "x86_64-apple-darwin"      # macOS (x86_64)
)
# Mac Catalyst (ios-macabi). Required for Xcode "My Mac (Mac Catalyst)".
if [[ "${INCLUDE_MACCATALYST:-1}" == "1" ]]; then
  APPLE_TARGETS+=(
    "aarch64-apple-ios-macabi"
    "x86_64-apple-ios-macabi"
  )
fi
# Optionally include Intel simulator (x86_64) slice
if [[ "${INCLUDE_INTEL_SIM:-0}" == "1" ]]; then
  APPLE_TARGETS+=( "x86_64-apple-ios" )  # iOS simulator (x86_64)
fi

# -----------------------------
# Helpers
# -----------------------------

die() {
  echo "error: $*" >&2
  exit 1
}

check_tools() {
  [[ -n "${XCODEBUILD_BIN}" ]] || die "xcodebuild not found (install Xcode or command line tools)"
  [[ -n "${CARGO_BIN}" ]] || die "cargo not found (install Rust toolchain: https://rustup.rs/)"
  [[ -f "${CRATE_DIR}/Cargo.toml" ]] || die "Cargo.toml not found at ${CRATE_DIR}"
  [[ -f "${HEADER_SRC}" ]] || die "C header not found at ${HEADER_SRC}"
}

ensure_targets() {
  if [[ -z "${RUSTUP_BIN}" ]]; then
    echo "warning: rustup not found, assuming required Apple targets are already installed"
    return
  fi
  for t in "${APPLE_TARGETS[@]}"; do
    echo "• Ensuring Rust target '${t}' is installed"
    "${RUSTUP_BIN}" target add "${t}" >/dev/null 2>&1 || true
  done
}

build_target() {
  local triple="$1"
  echo "• Building monerowalletcore for ${triple} (${PROFILE})"
  # Cargo requires the manifest to be named `Cargo.toml`, so we do not pass --manifest-path here.
  local extra_env=()
  case "${triple}" in
    *-apple-ios-macabi)
      # Catalyst links against the macOS SDK with an ios-macabi target triple.
      extra_env+=(
        "SDKROOT=$(xcrun --sdk macosx --show-sdk-path)"
        "MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET}"
      )
      ;;
  esac
  if [[ "${#extra_env[@]}" -gt 0 ]]; then
    (cd "${CRATE_DIR}" && env "${extra_env[@]}" "${CARGO_BIN}" build ${CARGO_PROFILE_FLAG} ${CARGO_FEATURES_FLAG} --target "${triple}")
  else
    (cd "${CRATE_DIR}" && "${CARGO_BIN}" build ${CARGO_PROFILE_FLAG} ${CARGO_FEATURES_FLAG} --target "${triple}")
  fi
}

lib_path_for() {
  local triple="$1"
  echo "${CRATE_DIR}/target/${triple}/${PROFILE}/libmonerowalletcore.a"
}

prepare_headers_dir() {
  local hdr_dir="$1"
  mkdir -p "${hdr_dir}" || die "failed to create headers dir ${hdr_dir}"

  # Copy the public header
  cp -f "${HEADER_SRC}" "${hdr_dir}/monerowalletcore.h" || die "failed to copy monerowalletcore.h"

  # Generate a module.modulemap so Swift can import the C module as 'MoneroWalletCore'
  cat > "${hdr_dir}/module.modulemap" <<'EOF'
module MoneroWalletCore [system] {
  header "monerowalletcore.h"
  export *
  link "monerowalletcore"
}
EOF
}

create_xcframework() {
  local args=()
  local hdr_dir="$1"
  shift
  local libs=( "$@" )

  # Only include libraries that exist
  local included=0
  for lib in "${libs[@]}"; do
    if [[ -f "${lib}" ]]; then
      echo "• Including library: ${lib}"
      args+=( -library "${lib}" -headers "${hdr_dir}" )
      included=$((included + 1))
    else
      echo "• Skipping missing library: ${lib}"
    fi
  done

  [[ "${included}" -gt 0 ]] || die "no libraries found to include in xcframework"

  # Clean previous output
  rm -rf "${XCFRAMEWORK_PATH}"
  mkdir -p "${OUT_DIR}"

  echo "• Creating xcframework at ${XCFRAMEWORK_PATH}"
  set +e
  "${XCODEBUILD_BIN}" -create-xcframework \
    "${args[@]}" \
    -output "${XCFRAMEWORK_PATH}"
  XC_STATUS=$?
  set -e
  if [[ "${XC_STATUS}" -ne 0 ]]; then
    die "xcodebuild -create-xcframework failed with exit code ${XC_STATUS}"
  fi
  if [[ ! -d "${XCFRAMEWORK_PATH}" ]]; then
    die "xcframework not created at ${XCFRAMEWORK_PATH}"
  fi
  echo "• xcframework created:"
  ls -lah "${XCFRAMEWORK_PATH%/*}" || true
}

# -----------------------------
# Main
# -----------------------------

echo "== MoneroWalletCore xcframework build =="
echo "  - Repo root: ${REPO_ROOT}"
echo "  - Crate dir: ${CRATE_DIR}"
echo "  - Output dir: ${OUT_DIR}"
echo "  - Profile: ${PROFILE}"

check_tools

ensure_targets

# Build all Apple targets
for target in "${APPLE_TARGETS[@]}"; do
  build_target "${target}"
done

# Collect built libraries
LIB_IOS_ARM64="$(lib_path_for aarch64-apple-ios)"
LIB_IOS_SIM_ARM64="$(lib_path_for aarch64-apple-ios-sim)"
LIB_IOS_SIM_X86_64="$(lib_path_for x86_64-apple-ios)"
LIB_MAC_ARM64="$(lib_path_for aarch64-apple-darwin)"
LIB_MAC_X86_64="$(lib_path_for x86_64-apple-darwin)"
LIB_CATALYST_ARM64="$(lib_path_for aarch64-apple-ios-macabi)"
LIB_CATALYST_X86_64="$(lib_path_for x86_64-apple-ios-macabi)"

# Prepare a temporary headers folder
TMPDIR="$(mktemp -d /tmp/monerowalletcore.xc.XXXXXX)"
trap 'rm -rf "${TMPDIR}"' EXIT
HDRS="${TMPDIR}/Headers"
prepare_headers_dir "${HDRS}"

# Create fat/universal libs to avoid "equivalent library definitions"
UNIVERSAL_LIBS=()

# iOS device (arm64 only)
if [[ -f "${LIB_IOS_ARM64}" ]]; then
  UNIVERSAL_LIBS+=( "${LIB_IOS_ARM64}" )
fi

# iOS simulator: if both arm64 and x86_64 exist, lipo to a single universal; otherwise include whichever exists
SIM_UNIV="${TMPDIR}/libmonerowalletcore_ios_sim_universal.a"
if [[ -f "${LIB_IOS_SIM_ARM64}" && -f "${LIB_IOS_SIM_X86_64}" ]]; then
  echo "• Creating iOS simulator universal lib via lipo"
  lipo -create -output "${SIM_UNIV}" "${LIB_IOS_SIM_ARM64}" "${LIB_IOS_SIM_X86_64}"
  UNIVERSAL_LIBS+=( "${SIM_UNIV}" )
else
  if [[ -f "${LIB_IOS_SIM_ARM64}" ]]; then
    UNIVERSAL_LIBS+=( "${LIB_IOS_SIM_ARM64}" )
  elif [[ -f "${LIB_IOS_SIM_X86_64}" ]]; then
    UNIVERSAL_LIBS+=( "${LIB_IOS_SIM_X86_64}" )
  fi
fi

# macOS: combine arm64 + x86_64 into one universal lib
MAC_UNIV="${TMPDIR}/libmonerowalletcore_macos_universal.a"
if [[ -f "${LIB_MAC_ARM64}" && -f "${LIB_MAC_X86_64}" ]]; then
  echo "• Creating macOS universal lib via lipo"
  lipo -create -output "${MAC_UNIV}" "${LIB_MAC_ARM64}" "${LIB_MAC_X86_64}"
  UNIVERSAL_LIBS+=( "${MAC_UNIV}" )
elif [[ -f "${LIB_MAC_ARM64}" ]]; then
  UNIVERSAL_LIBS+=( "${LIB_MAC_ARM64}" )
elif [[ -f "${LIB_MAC_X86_64}" ]]; then
  UNIVERSAL_LIBS+=( "${LIB_MAC_X86_64}" )
fi

# Mac Catalyst: lipo arm64 + x86_64 macabi into one slice
CATALYST_UNIV="${TMPDIR}/libmonerowalletcore_maccatalyst_universal.a"
if [[ -f "${LIB_CATALYST_ARM64}" && -f "${LIB_CATALYST_X86_64}" ]]; then
  echo "• Creating Mac Catalyst universal lib via lipo"
  lipo -create -output "${CATALYST_UNIV}" "${LIB_CATALYST_ARM64}" "${LIB_CATALYST_X86_64}"
  UNIVERSAL_LIBS+=( "${CATALYST_UNIV}" )
elif [[ -f "${LIB_CATALYST_ARM64}" ]]; then
  UNIVERSAL_LIBS+=( "${LIB_CATALYST_ARM64}" )
elif [[ -f "${LIB_CATALYST_X86_64}" ]]; then
  UNIVERSAL_LIBS+=( "${LIB_CATALYST_X86_64}" )
fi

# Create the xcframework with one library per platform/variant
create_xcframework "${HDRS}" "${UNIVERSAL_LIBS[@]}"

echo ""
echo "== Done =="
echo "Generated: ${XCFRAMEWORK_PATH}"
echo ""
echo "Package it for a WalletCore release with:"
echo "  ARTIFACTS_DIR=\"${OUT_DIR}\" ./Scripts/package_release_artifacts.sh"
echo ""
echo "The generated XCFramework is intentionally kept under .build/ and is not committed."
