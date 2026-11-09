#!/usr/bin/env bash
#
# Builds the signed, notarized universal macOS release into release/<version>/.
# Everything is assembled in a work directory and moved there only at the very
# end, so a failed run leaves an earlier release/<version>/ as it was.
#
#   scripts/release.sh [--skip-notarize]
#
# --skip-notarize is for trying out a build: the DMG is named *_UNNOTARIZED.dmg,
# no checksum file is written, and the result must not be shipped.
#
# Needs the Developer ID certificate in the keychain and, unless --skip-notarize
# is given, a notarytool keychain profile. No credential is read from this
# repository or from the environment.

set -euo pipefail

IDENTITY="Developer ID Application: Konstantin Penzin (9R2KGM7B52)"
TEAM_ID="9R2KGM7B52"
NOTARY_PROFILE="porthole-notary"
TARGET="universal-apple-darwin"

die() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  echo "usage: scripts/release.sh [--skip-notarize]"
}

NOTARIZE=1
for arg in "$@"; do
  case "$arg" in
    --skip-notarize) NOTARIZE=0 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown argument: $arg"
      ;;
  esac
done

cd "$(dirname "$0")/.."

config_value() {
  node -p "require('./src-tauri/tauri.conf.json').$1"
}

VERSION="$(config_value version)"
PRODUCT="$(config_value productName)"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] ||
  die "unexpected version in src-tauri/tauri.conf.json: $VERSION"

OUT="release/$VERSION"
BUILT_APP="src-tauri/target/$TARGET/release/bundle/macos/$PRODUCT.app"

# Captured first: with pipefail, `grep -q` closing the pipe early would make
# `security` fail and report a present identity as missing.
identities="$(security find-identity -v -p codesigning)"
grep -qF "\"$IDENTITY\"" <<<"$identities" ||
  die "signing identity not found in the keychain: $IDENTITY"

if [[ "$NOTARIZE" == 1 ]] &&
  ! notary_output="$(xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" 2>&1)"; then
  die "notarytool could not use the keychain profile '$NOTARY_PROFILE':
$notary_output
If the profile does not exist, create it with:
  xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id <apple-id> --team-id $TEAM_ID
It prompts for an app-specific password."
fi

# Inside release/ so that the final move is a rename on the same volume.
mkdir -p release
WORK="$(mktemp -d "$PWD/release/.work-XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

STAGE="$WORK/out"
mkdir "$STAGE"
APP="$STAGE/$PRODUCT.app"
if [[ "$NOTARIZE" == 1 ]]; then
  DMG="$STAGE/${PRODUCT}_${VERSION}_universal.dmg"
else
  DMG="$STAGE/${PRODUCT}_${VERSION}_universal_UNNOTARIZED.dmg"
fi

json_field() {
  node -e '
    let text = "";
    process.stdin.on("data", (chunk) => (text += chunk)).on("end", () => {
      let value;
      try { value = JSON.parse(text)[process.argv[1]]; } catch {}
      process.stdout.write(value == null ? "" : String(value));
    });
  ' "$1"
}

# Submits a file and fails unless Apple accepts it. The status is read from the
# result because a finished submission can be rejected.
notarize() {
  local file="$1" result id status
  echo "==> Notarizing $(basename "$file")"
  result="$(xcrun notarytool submit "$file" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json)" || true
  id="$(json_field id <<<"$result")"
  status="$(json_field status <<<"$result")"
  echo "    submission ${id:-unknown}: ${status:-no status}"
  if [[ "$status" != "Accepted" ]]; then
    echo "$result" >&2
    [[ -z "$id" ]] || xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
    die "notarization of $(basename "$file") failed"
  fi
}

staple() {
  echo "==> Stapling $(basename "$1")"
  xcrun stapler staple "$1"
  xcrun stapler validate "$1"
}

echo "==> Building $PRODUCT $VERSION for $TARGET"
# Tauri notarizes on its own when these are set, and cannot use a notarytool
# profile. The app is notarized below instead.
unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_KEY_PATH \
  APPLE_API_ISSUER APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD
# Only the app: a DMG made at this point would hold the app before stapling.
APPLE_SIGNING_IDENTITY="$IDENTITY" npm run tauri build -- --target "$TARGET" --bundles app

[[ -d "$BUILT_APP" ]] || die "build finished without producing $BUILT_APP"
ditto "$BUILT_APP" "$APP"

echo "==> Checking the app"
codesign --verify --deep --strict "$APP"
signature="$(codesign -dv --verbose=4 "$APP" 2>&1)"
grep -q '^Timestamp=' <<<"$signature" || die "$APP has no secure timestamp"
executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Contents/Info.plist")"
for arch in x86_64 arm64; do
  lipo "$APP/Contents/MacOS/$executable" -verify_arch "$arch" ||
    die "$executable has no $arch slice"
done

if [[ "$NOTARIZE" == 1 ]]; then
  ditto -c -k --keepParent "$APP" "$WORK/$PRODUCT.zip"
  notarize "$WORK/$PRODUCT.zip"
  staple "$APP"
fi

echo "==> Building $(basename "$DMG")"
mkdir "$WORK/dmg"
ditto "$APP" "$WORK/dmg/$PRODUCT.app"
ln -s /Applications "$WORK/dmg/Applications"
# hdiutil occasionally fails with "Resource busy" right after a volume operation.
attempt=1
until hdiutil create -volname "$PRODUCT" -srcfolder "$WORK/dmg" -fs HFS+ -format UDZO -ov "$DMG"; do
  [[ "$attempt" -lt 3 ]] || die "hdiutil could not create $DMG"
  attempt=$((attempt + 1))
  sleep 3
done
codesign --force --sign "$IDENTITY" --timestamp "$DMG"
codesign --verify --strict "$DMG"

if [[ "$NOTARIZE" == 1 ]]; then
  notarize "$DMG"
  staple "$DMG"
fi

if [[ "$NOTARIZE" == 1 ]]; then
  echo "==> Verifying the stapled app and the DMG"
  codesign --verify --deep --strict "$APP"
  spctl -a -t open --context context:primary-signature "$DMG"
  (cd "$STAGE" && shasum -a 256 "$(basename "$DMG")" >SHA256SUMS.txt)
fi

# The very last step. The old release is only set aside until the new one is in
# place, and comes back if the move fails.
if [[ -e "$OUT" ]]; then
  mv "$OUT" "$WORK/previous"
fi
if ! mv "$STAGE" "$OUT"; then
  if [[ -e "$WORK/previous" ]]; then
    mv "$WORK/previous" "$OUT"
  fi
  die "could not move the build to $OUT"
fi

echo "==> Done: $OUT"
ls -1 "$OUT"
if [[ "$NOTARIZE" == 0 ]]; then
  echo "warning: --skip-notarize was given: this build is signed but NOT notarized and must not be shipped." >&2
fi
