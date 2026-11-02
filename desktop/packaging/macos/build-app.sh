#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds a macOS app bundle for NectarLink Desktop:
#   target/macos/NectarLink.app
# with the Qt frameworks and plugins it needs (macdeployqt), an Info.plist
# carrying the privacy strings macOS asks for, the app icon and a signature.
#
#   desktop/packaging/macos/build-app.sh            # release build, ad-hoc signature
#   PROFILE=debug desktop/packaging/macos/build-app.sh
#   SIGN_IDENTITY="Developer ID Application: ..." desktop/packaging/macos/build-app.sh
#
# An ad-hoc signature runs on this Mac only. A Developer ID identity (plus
# notarization) is what other Macs, native notifications and a keychain item
# that survives updates without prompting need.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
DESKTOP_DIR="${REPO_ROOT}/desktop/app"

VERSION="${VERSION:-$(awk -F'"' '/^version[[:space:]]*=[[:space:]]*"/ { print $2; exit }' "${REPO_ROOT}/Cargo.toml")}"
if [[ -z "${VERSION}" ]]; then
    echo "error: could not determine version from ${REPO_ROOT}/Cargo.toml" >&2
    exit 1
fi

PROFILE="${PROFILE:-release}"
BUNDLE_ID="${BUNDLE_ID:-app.nectarlink.desktop}"
SIGN_IDENTITY="${SIGN_IDENTITY:--}"
DIST_DIR="${DIST_DIR:-${REPO_ROOT}/target/macos}"
APP="${DIST_DIR}/NectarLink.app"

if [[ -z "${QMAKE:-}" ]]; then
    for candidate in "${QT_ROOT_DIR:-}/bin/qmake" /opt/homebrew/opt/qt/bin/qmake /usr/local/opt/qt/bin/qmake; do
        if [[ -x "${candidate}" ]]; then
            export QMAKE="${candidate}"
            break
        fi
    done
fi
if [[ -z "${QMAKE:-}" ]]; then
    echo "error: Qt 6 qmake not found; set QMAKE or QT_ROOT_DIR (brew install qt)" >&2
    exit 1
fi
MACDEPLOYQT="$(dirname "${QMAKE}")/macdeployqt"

echo "==> Building nectarlink-desktop (${PROFILE})"
if [[ "${PROFILE}" == "release" ]]; then
    cargo build --locked --release -p nectarlink-desktop --manifest-path "${REPO_ROOT}/Cargo.toml"
else
    cargo build --locked -p nectarlink-desktop --manifest-path "${REPO_ROOT}/Cargo.toml"
fi
BIN_PATH="${REPO_ROOT}/target/${PROFILE}/nectarlink-desktop"

echo "==> Assembling ${APP}"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
cp "${BIN_PATH}" "${APP}/Contents/MacOS/nectarlink-desktop"

# The icon set iconutil expects, from the 512 px brand icon.
ICONSET="$(mktemp -d)/NectarLink.iconset"
mkdir -p "${ICONSET}"
for size in 16 32 128 256 512; do
    sips -z "${size}" "${size}" "${REPO_ROOT}/assets/brand/icon-512.png" --out "${ICONSET}/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    if [[ "${double}" -le 512 ]]; then
        sips -z "${double}" "${double}" "${REPO_ROOT}/assets/brand/icon-512.png" --out "${ICONSET}/icon_${size}x${size}@2x.png" >/dev/null
    fi
done
iconutil -c icns "${ICONSET}" -o "${APP}/Contents/Resources/NectarLink.icns"
rm -rf "$(dirname "${ICONSET}")"

# NSLocalNetworkUsageDescription: without it macOS 15+ silently blocks the
# LAN discovery and connections of a bundled app (a bare binary borrows the
# terminal's permission). NSAppleEventsUsageDescription: locking the Mac goes
# through System Events.
cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>NectarLink</string>
    <key>CFBundleExecutable</key><string>nectarlink-desktop</string>
    <key>CFBundleIconFile</key><string>NectarLink</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>NectarLink</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>NectarLink finds your phone and connects to it directly on your local network.</string>
    <key>NSBonjourServices</key>
    <array>
        <string>_irohv1._udp</string>
        <string>_adb-tls-connect._tcp</string>
    </array>
    <key>NSAppleEventsUsageDescription</key>
    <string>NectarLink asks System Events to lock this Mac when you lock it from your phone.</string>
</dict>
</plist>
PLIST

echo "==> Bundling Qt (${MACDEPLOYQT})"
# Homebrew splits Qt into one keg per module; QT_INSTALL_LIBS links them all.
"${MACDEPLOYQT}" "${APP}" -qmldir="${DESKTOP_DIR}/qml" -libpath="$("${QMAKE}" -query QT_INSTALL_LIBS)" -verbose=1

# macdeployqt copies every plugin of a module, including ones for Qt modules
# NectarLink doesn't use (PDF image format, virtual keyboard); drop those whose
# frameworks weren't bundled, or Qt fails to load them at runtime.
while IFS= read -r plugin; do
    missing="$(otool -L "${plugin}" | awk '/@rpath\/Qt[A-Za-z0-9]+\.framework/ { split($1, p, "/"); print p[2] }' |
        while read -r framework; do
            [[ -d "${APP}/Contents/Frameworks/${framework}" ]] || echo "${framework}"
        done)"
    if [[ -n "${missing}" ]]; then
        echo "    dropping ${plugin#"${APP}/Contents/"} (needs $(echo "${missing}" | tr '\n' ' '))"
        rm -f "${plugin}"
    fi
done < <(find "${APP}/Contents/PlugIns" -name '*.dylib')
# Their QML modules link to the dropped plugins; remove them too, or the
# dangling links fail strict signature verification.
while IFS= read -r link; do
    rm -rf "$(dirname "${link}")"
done < <(find "${APP}/Contents/Resources/qml" -type l ! -exec test -e {} \; -print)

echo "==> Signing (${SIGN_IDENTITY})"
if [[ "${SIGN_IDENTITY}" == "-" ]]; then
    codesign --force --deep --sign - "${APP}"
else
    codesign --force --deep --options runtime --timestamp --sign "${SIGN_IDENTITY}" "${APP}"
fi
codesign --verify --deep --strict "${APP}"

echo "==> ${APP} ($(du -sh "${APP}" | cut -f1))"
