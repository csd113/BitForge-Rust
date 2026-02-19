#!/usr/bin/env bash
# build.sh — Compile BitForge and assemble a macOS .app bundle.
#
# Usage:
#   ./build.sh                  # release build for the current arch
#   ./build.sh --debug          # debug build (faster, larger binary)
#   ./build.sh --universal      # fat binary: arm64 + x86_64
#   ./build.sh --sign "Developer ID Application: You (TEAMID)"
#
# Output: ./dist/BitForge.app
#
# Prerequisites:
#   • Rust toolchain (rustup)
#   • Xcode Command Line Tools  (xcode-select --install)
#   • For --universal: both targets installed via rustup

set -euo pipefail

# ── Configurable identifiers ──────────────────────────────────────────────────
APP_NAME="BitForge"
BINARY_NAME="bitcoin-compiler"          # must match [[bin]] name in Cargo.toml
BUNDLE_ID="com.bitcoincompiler.app"
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
MINIMUM_MACOS="12.0"

# ── Argument parsing ──────────────────────────────────────────────────────────
BUILD_MODE="release"
UNIVERSAL=false
SIGN_IDENTITY=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)     BUILD_MODE="debug" ;;
        --universal) UNIVERSAL=true ;;
        --sign)      shift; SIGN_IDENTITY="$1" ;;
        *)           echo "Unknown argument: $1"; exit 1 ;;
    esac
    shift
done

# ── Detect host architecture ──────────────────────────────────────────────────
HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in
    arm64)  RUST_TARGET="aarch64-apple-darwin" ;;
    x86_64) RUST_TARGET="x86_64-apple-darwin" ;;
    *)      echo "Unsupported architecture: $HOST_ARCH"; exit 1 ;;
esac

# ── Paths ─────────────────────────────────────────────────────────────────────
DIST_DIR="dist"
APP_DIR="$DIST_DIR/${APP_NAME}.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

# ── Helpers ───────────────────────────────────────────────────────────────────
info()    { printf '\033[1;34m==> \033[0m%s\n' "$*"; }
success() { printf '\033[1;32m✓ \033[0m%s\n' "$*"; }
warn()    { printf '\033[1;33m⚠  \033[0m%s\n' "$*"; }
err()     { printf '\033[1;31m✗ \033[0m%s\n' "$*" >&2; exit 1; }

# ── Preflight checks ──────────────────────────────────────────────────────────
info "BitForge ${VERSION} — build mode: ${BUILD_MODE}${UNIVERSAL:+ (universal)}"

command -v cargo  >/dev/null 2>&1 || err "cargo not found — install Rust from https://rustup.rs"
command -v codesign >/dev/null 2>&1 || err "codesign not found — install Xcode Command Line Tools"

if $UNIVERSAL; then
    rustup target list --installed | grep -q "aarch64-apple-darwin" \
        || err "Missing target. Run: rustup target add aarch64-apple-darwin"
    rustup target list --installed | grep -q "x86_64-apple-darwin" \
        || err "Missing target. Run: rustup target add x86_64-apple-darwin"
fi

# ── Compile ───────────────────────────────────────────────────────────────────
CARGO_FLAGS=()
[[ "$BUILD_MODE" == "release" ]] && CARGO_FLAGS+=(--release)

if $UNIVERSAL; then
    info "Compiling for aarch64-apple-darwin..."
    cargo build "${CARGO_FLAGS[@]}" --target aarch64-apple-darwin

    info "Compiling for x86_64-apple-darwin..."
    cargo build "${CARGO_FLAGS[@]}" --target x86_64-apple-darwin

    ARM_BIN="target/aarch64-apple-darwin/${BUILD_MODE}/${BINARY_NAME}"
    X86_BIN="target/x86_64-apple-darwin/${BUILD_MODE}/${BINARY_NAME}"
    BUILT_BINARY="target/${BUILD_MODE}/${BINARY_NAME}_universal"

    info "Fusing universal binary with lipo..."
    lipo -create "$ARM_BIN" "$X86_BIN" -output "$BUILT_BINARY"
else
    info "Compiling for ${RUST_TARGET}..."
    cargo build "${CARGO_FLAGS[@]}" --target "$RUST_TARGET"
    BUILT_BINARY="target/${RUST_TARGET}/${BUILD_MODE}/${BINARY_NAME}"
fi

[[ -f "$BUILT_BINARY" ]] || err "Build succeeded but binary not found at: $BUILT_BINARY"
success "Binary built: $BUILT_BINARY ($(du -sh "$BUILT_BINARY" | cut -f1))"

# ── Assemble .app bundle ──────────────────────────────────────────────────────
info "Assembling ${APP_NAME}.app..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

# Binary
cp "$BUILT_BINARY" "$MACOS_DIR/${APP_NAME}"
chmod 755 "$MACOS_DIR/${APP_NAME}"

# Copy icon if it exists (icns format expected)
if [[ -f "assets/${APP_NAME}.icns" ]]; then
    cp "assets/${APP_NAME}.icns" "$RESOURCES_DIR/${APP_NAME}.icns"
    ICON_FILE_LINE="<key>CFBundleIconFile</key><string>${APP_NAME}</string>"
else
    warn "No icon found at assets/${APP_NAME}.icns — bundle will use default icon"
    ICON_FILE_LINE=""
fi

# Info.plist
cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>

    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>

    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>

    <key>CFBundleVersion</key>
    <string>${VERSION}</string>

    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>

    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>

    <key>CFBundlePackageType</key>
    <string>APPL</string>

    <key>CFBundleSignature</key>
    <string>????</string>

    <key>LSMinimumSystemVersion</key>
    <string>${MINIMUM_MACOS}</string>

    <key>NSHighResolutionCapable</key>
    <true/>

    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>

    <key>CFBundleSupportedPlatforms</key>
    <array>
        <string>MacOSX</string>
    </array>

    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>

    <key>NSHumanReadableCopyright</key>
    <string>Copyright © 2024 Bitcoin Compiler App. MIT License.</string>

    ${ICON_FILE_LINE}
</dict>
</plist>
PLIST

success "Bundle assembled: $APP_DIR"

# ── Codesign ──────────────────────────────────────────────────────────────────
if [[ -n "$SIGN_IDENTITY" ]]; then
    info "Signing with identity: ${SIGN_IDENTITY}"
    codesign \
        --force \
        --deep \
        --sign "$SIGN_IDENTITY" \
        --options runtime \
        --timestamp \
        "$APP_DIR"
    codesign --verify --verbose "$APP_DIR"
    success "Signed with Developer ID"
else
    info "Applying ad-hoc codesign (local use only)..."
    codesign --force --deep --sign "-" "$APP_DIR"
    success "Ad-hoc signed — app will run on this Mac without Gatekeeper prompts"
    warn "For distribution, re-run with: --sign \"Developer ID Application: You (TEAMID)\""
fi

# ── Summary ───────────────────────────────────────────────────────────────────
BUNDLE_SIZE=$(du -sh "$APP_DIR" | cut -f1)
echo ""
printf '\033[1;32m'
echo "╔══════════════════════════════════════════════════╗"
echo "║           BitForge build complete! 🎉            ║"
echo "╚══════════════════════════════════════════════════╝"
printf '\033[0m'
echo ""
echo "  App bundle : $APP_DIR"
echo "  Version    : $VERSION"
echo "  Size       : $BUNDLE_SIZE"
echo "  Mode       : $BUILD_MODE${UNIVERSAL:+ (universal)}"
echo ""
echo "  To run:"
echo "    open $APP_DIR"
echo ""
echo "  To notarise for distribution:"
echo "    xcrun notarytool submit $APP_DIR \\"
echo "      --apple-id you@example.com --team-id TEAMID \\"
echo "      --password APP_SPECIFIC_PASSWORD --wait"
echo "    xcrun stapler staple $APP_DIR"
echo ""
