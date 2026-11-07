#!/usr/bin/env bash
set -euo pipefail

# Build the Rust local agent core (mobile/rust) for iOS and Android.
#
# Usage:
#   ./scripts/build-rust.sh              # build for all targets
#   ./scripts/build-rust.sh ios          # iOS XCFramework (device + simulator)
#   ./scripts/build-rust.sh android      # Android .so files (requires the NDK)
#
# iOS output is modules/cw-rust-bridge/ios/CWMobile.xcframework, which the local
# Expo module vendors. It is committed: EAS builders have no Rust toolchain, so
# rebuild and commit it whenever mobile/rust changes.
#
# The XCFramework holds one static CWMobile.framework per platform:
#   ios-arm64                      device
#   ios-arm64_x86_64-simulator     Apple-silicon and Intel simulators
# A single .a cannot carry both arm64 slices (device and simulator arm64 are
# different platforms with the same architecture), which is why the earlier
# lipo step failed silently and left a device-only library.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MOBILE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RUST_DIR="$MOBILE_DIR/rust"
IOS_OUT="$MOBILE_DIR/modules/cw-rust-bridge/ios/CWMobile.xcframework"
ANDROID_OUT="$MOBILE_DIR/native/rust/android"

# Matches the app's deployment target (Expo SDK 57's minimum), so the linker
# never sees objects built for a newer iOS than the app.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.4}"

cd "$RUST_DIR"

echo "==> Building Rust mobile agent core"

# ── iOS ───────────────────────────────────────────────────────────────

# Wrap a static library as CWMobile.framework so Swift can `import CWMobile`.
make_framework() {
  local lib="$1" dir="$2/CWMobile.framework"
  local version
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
  mkdir -p "$dir/Headers" "$dir/Modules"
  cp "$lib" "$dir/CWMobile"
  cp include/cw_mobile.h "$dir/Headers/"
  cat > "$dir/Modules/module.modulemap" <<'EOF'
framework module CWMobile {
  umbrella header "cw_mobile.h"
  export *
}
EOF
  cat > "$dir/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>CWMobile</string>
  <key>CFBundleIdentifier</key>
  <string>world.consistent.CWMobile</string>
  <key>CFBundleName</key>
  <string>CWMobile</string>
  <key>CFBundlePackageType</key>
  <string>FMWK</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>MinimumOSVersion</key>
  <string>$IPHONEOS_DEPLOYMENT_TARGET</string>
</dict>
</plist>
EOF
}

build_ios() {
  local targets=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
  echo "==> iOS: ${targets[*]} (min iOS $IPHONEOS_DEPLOYMENT_TARGET)"
  if command -v rustup >/dev/null; then
    rustup target add "${targets[@]}"
  fi
  for t in "${targets[@]}"; do
    cargo build --release --target "$t"
  done

  local stage="target/xcframework"
  rm -rf "$stage"
  mkdir -p "$stage/device" "$stage/simulator"
  cp target/aarch64-apple-ios/release/libcw_mobile.a "$stage/device/libcw_mobile.a"
  lipo -create \
    target/aarch64-apple-ios-sim/release/libcw_mobile.a \
    target/x86_64-apple-ios/release/libcw_mobile.a \
    -output "$stage/simulator/libcw_mobile.a"
  # The prebuilt std objects carry DWARF that the release profile can't drop
  # from a staticlib; stripping it takes each slice from ~5.8 MB to ~2.3 MB.
  # Exported symbols (the cw_* functions) are kept.
  xcrun strip -S "$stage/device/libcw_mobile.a" "$stage/simulator/libcw_mobile.a"

  make_framework "$stage/device/libcw_mobile.a" "$stage/device"
  make_framework "$stage/simulator/libcw_mobile.a" "$stage/simulator"

  rm -rf "$IOS_OUT"
  xcodebuild -create-xcframework \
    -framework "$stage/device/CWMobile.framework" \
    -framework "$stage/simulator/CWMobile.framework" \
    -output "$IOS_OUT"

  echo "==> iOS XCFramework: $IOS_OUT"
  for slice in "$IOS_OUT"/*/CWMobile.framework/CWMobile; do
    lipo -info "$slice"
  done
  du -sh "$IOS_OUT"
}

# ── Android ───────────────────────────────────────────────────────────
build_android() {
  echo "==> Android: aarch64-linux-android + x86_64-linux-android"
  if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    echo "WARNING: ANDROID_NDK_HOME not set — skipping Android"
    return
  fi
  cargo build --target aarch64-linux-android --release
  cargo build --target x86_64-linux-android --release

  mkdir -p "$ANDROID_OUT/arm64-v8a"
  mkdir -p "$ANDROID_OUT/x86_64"
  cp "target/aarch64-linux-android/release/libcw_mobile.so" "$ANDROID_OUT/arm64-v8a/"
  cp "target/x86_64-linux-android/release/libcw_mobile.so" "$ANDROID_OUT/x86_64/"

  echo "==> Android libraries: $ANDROID_OUT/"
}

TARGET="${1:-all}"
case "$TARGET" in
  ios) build_ios ;;
  android) build_android ;;
  all) build_ios; build_android ;;
  *) echo "Unknown target: $TARGET (use ios, android, or all)" ; exit 1 ;;
esac
