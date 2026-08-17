#!/bin/bash
# =============================================================================
# scripts/make-dmg.sh
#   Wrap a SIGNED Kod.app into the artifact people actually download: a signed,
#   notarized, stapled .dmg with the conventional drag-to-Applications layout.
#
#   Usage:
#       ./scripts/make-dmg.sh                       # sign + notarize + staple
#       SKIP_NOTARIZE=1 ./scripts/make-dmg.sh       # local smoke test, no Apple
#
#   Env:  SIGN_IDENTITY   required unless SKIP_NOTARIZE (same string as
#                         sign-notarize.sh; see: security find-identity -v -p codesigning)
#         NOTARY_PROFILE  keychain profile name (default: kod-notary)
#
# -----------------------------------------------------------------------------
# RUN scripts/sign-notarize.sh FIRST. This script deliberately does NOT build or
# sign the .app — it refuses to package an unsigned one, because a DMG is only as
# trustworthy as what is inside it and a silently-unsigned payload is the exact
# mistake worth making impossible.
#
# WHY THE DMG IS NOTARIZED SEPARATELY: notarization applies to the artifact you
# ship. Notarizing the .app proves the app; it does not staple a ticket to the
# .dmg, so a downloaded DMG would still make Gatekeeper phone home (and fail
# closed offline). Stapling the DMG is what makes the download open cleanly with
# no network. The app inside keeps its own ticket, so both are covered.
# =============================================================================

set -euo pipefail

cd "$(cd "$(dirname "$0")/.." && pwd)"

APP="../Kod.app"
DIST="../dist"
VOL="Kod"
DMG="$DIST/Kod.dmg"
NOTARY_PROFILE="${NOTARY_PROFILE:-kod-notary}"

[ -d "$APP" ] || { echo "error: $APP not found — run scripts/sign-notarize.sh first" >&2; exit 1; }

# --- refuse to ship an unsigned payload -------------------------------------
# NOTE --verbose=2 is REQUIRED: plain `codesign -dv` prints the CodeDirectory and
# signature size but NO Authority= lines at all, so grepping its default output
# rejects every correctly signed app. Verified both ways before trusting it.
# `codesign --verify --strict` fails on an unsigned or broken bundle. Checking
# for a Developer ID authority specifically, because an ad-hoc signature also
# passes --verify and would sail through to a DMG nobody else can open.
if ! codesign --verify --strict "$APP" 2>/dev/null; then
  echo "error: $APP is not validly signed — run scripts/sign-notarize.sh first" >&2
  exit 1
fi
# Captured into a variable and matched with `case`, NOT piped into `grep -q`.
# Under `set -o pipefail` that pipeline reports FAILURE when the match SUCCEEDS:
# grep -q exits at the first hit, closing the pipe, codesign dies of SIGPIPE
# (141), and pipefail propagates 141 as the pipeline's status. The guard then
# rejects every correctly signed app — for the one reason hardest to guess from
# its error message. `case` on a string does no I/O and cannot misfire.
SIGINFO="$(codesign -dv --verbose=2 "$APP" 2>&1 || true)"
case "$SIGINFO" in
  *"Authority=Developer ID Application"*) ;;
  *)
    echo "error: $APP is not signed with a Developer ID Application cert." >&2
    echo "       A DMG built from it would fail Gatekeeper on every other Mac." >&2
    exit 1
    ;;
esac
echo "==> payload verified: $(printf '%s\n' "$SIGINFO" | sed -n 's/^\(Authority=Developer ID Application.*\)/\1/p' | head -1)"

# --- stage the drag-to-install layout ---------------------------------------
# A /Applications SYMLINK, not a copy: the whole convention is that the user
# drags left onto right. hdiutil follows the symlink into the image, and it
# costs no bytes.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
/usr/bin/ditto "$APP" "$STAGE/Kod.app"     # ditto, not cp: preserves bundle symlinks + xattrs
ln -s /Applications "$STAGE/Applications"

mkdir -p "$DIST"
rm -f "$DMG"

echo "==> creating $DMG"
# UDZO = compressed read-only, the standard for distribution.
hdiutil create -volname "$VOL" -srcfolder "$STAGE" -ov -format UDZO -quiet "$DMG"

if [ -n "${SKIP_NOTARIZE:-}" ]; then
  echo "==> SKIP_NOTARIZE set — leaving $DMG unsigned and un-notarized (local test only)"
  echo "    DO NOT distribute this file."
  exit 0
fi

: "${SIGN_IDENTITY:?set SIGN_IDENTITY (see: security find-identity -v -p codesigning)}"

# --- sign the DMG itself ----------------------------------------------------
# The container gets its own signature; without it notarization rejects the
# submission outright.
echo "==> signing $DMG"
codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG"

echo "==> submitting $DMG to notarytool (waiting for verdict)"
SUBMIT_OUT="$(xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1)" || true
echo "$SUBMIT_OUT"
if ! printf '%s\n' "$SUBMIT_OUT" | grep -q 'status: Accepted'; then
  ID="$(printf '%s\n' "$SUBMIT_OUT" | grep -oE '\b[0-9a-fA-F-]{36}\b' | head -n1 || true)"
  echo "error: DMG notarization did not reach 'Accepted'." >&2
  [ -n "$ID" ] && xcrun notarytool log "$ID" --keychain-profile "$NOTARY_PROFILE" >&2 || true
  exit 1
fi

echo "==> stapling the ticket to $DMG"
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"

# --- prove what a downloader's Mac will decide ------------------------------
# Assessed as `open`, NOT `execute`: Gatekeeper evaluates a DMG under a different
# policy than an app bundle, and asking the wrong question returns a confidently
# wrong answer.
echo "==> Gatekeeper assessment (as a downloaded disk image)"
spctl --assess --type open --context context:primary-signature --verbose=4 "$DMG" 2>&1 | sed 's/^/    /'

echo ""
echo ">> done: $DMG"
echo "   size: $(du -h "$DMG" | cut -f1)"
echo "   this is the file to attach to a GitHub Release."
