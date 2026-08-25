#!/bin/bash
# =============================================================================
# scripts/sign-notarize.sh
#   Sign + notarize + staple Kod.app for Developer ID distribution.
#
#   THIS IS A RELEASE STEP, SEPARATE FROM SOURCE BUILDS.
#   - Plain source/dev builds use scripts/make-app.sh and stay entirely
#     credential-free. Source-builders (`cargo run`, an unsigned Kod.app) run
#     UNSIGNED: UNUserNotificationCenter is unavailable outside a signed bundle,
#     so Kod's notify path FALLS BACK to osascript. That is expected and fine.
#   - Only this script touches a code-signing identity or notarization creds,
#     and it reads them from the environment / the login keychain. NO SECRET is
#     ever written into this file or the repo.
#
# -----------------------------------------------------------------------------
# ONE-TIME SETUP THE SIGNER MUST PROVIDE (nothing here is committed):
#
#   1. Apple Developer Program membership.
#
#   2. A "Developer ID Application" certificate + private key in the LOGIN
#      keychain (download from the Apple Developer portal — this is the
#      Developer-ID cert, NOT the Mac App Store "3rd Party Mac Developer" one).
#      Find the exact identity string with:
#          security find-identity -v -p codesigning
#      then export it (of the form below, TEAMID is your 10-char Team ID):
#          export SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID10CH)"
#
#   3. A notarytool credential source. Pick ONE:
#
#      (a) RECOMMENDED — a stored keychain profile. Run ONCE, by hand (the
#          secret lands in the keychain, never in a file):
#              xcrun notarytool store-credentials "kod-notary" \
#                --apple-id <your-apple-id-email> \
#                --team-id  <TEAMID10CH> \
#                --password <app-specific-password>
#          The <app-specific-password> is generated at appleid.apple.com — it is
#          NOT your real Apple account password. This script defaults to the
#          "kod-notary" profile; override with:  export NOTARY_PROFILE=<name>
#
#      (b) OR inline Apple ID via env vars (used automatically if all three are
#          set; the password must be an app-specific password, supplied at run
#          time, never a literal in a file):
#              export AC_APPLE_ID="<your-apple-id-email>"
#              export AC_TEAM_ID="TEAMID10CH"
#              export AC_PASSWORD="<app-specific-password>"
#
#      (For an App Store Connect .p8 API key instead, swap the NOTARY_AUTH array
#       below for: --key <p8-path> --key-id <KEYID> --issuer <ISSUER-UUID>.)
#
# -----------------------------------------------------------------------------
# USAGE:
#   Build a fresh release bundle, then sign/notarize/staple it:
#       SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID10CH)" \
#         ./scripts/sign-notarize.sh
#
#   Reuse an already-assembled ../Kod.app (skip the cargo release build):
#       SKIP_BUILD=1 SIGN_IDENTITY="..." ./scripts/sign-notarize.sh
#
#   Env knobs:  SIGN_IDENTITY (required)  NOTARY_PROFILE (default kod-notary)
#               SKIP_BUILD (set to reuse existing ../Kod.app)
#               AC_APPLE_ID / AC_TEAM_ID / AC_PASSWORD (optional inline auth)
# =============================================================================
set -euo pipefail

# Run from the crate/workspace root (app/), same as make-app.sh.
cd "$(dirname "$0")/.."

APP="../Kod.app"                       # make-app.sh writes the bundle here
ENT="scripts/kod.entitlements"         # measured minimal entitlements (in repo)
DIST="../dist"
ZIP="$DIST/Kod.zip"
NOTARY_PROFILE="${NOTARY_PROFILE:-kod-notary}"

# --- Identity is required, never hardcoded ----------------------------------
: "${SIGN_IDENTITY:?set SIGN_IDENTITY to your 'Developer ID Application: Name (TEAMID)' (see: security find-identity -v -p codesigning)}"

# --- Pick the notarytool auth source (keychain profile by default) ----------
if [ -n "${AC_APPLE_ID:-}" ] && [ -n "${AC_TEAM_ID:-}" ] && [ -n "${AC_PASSWORD:-}" ]; then
  NOTARY_AUTH=(--apple-id "$AC_APPLE_ID" --team-id "$AC_TEAM_ID" --password "$AC_PASSWORD")
  echo "==> notarytool auth: inline Apple ID ($AC_APPLE_ID)"
else
  NOTARY_AUTH=(--keychain-profile "$NOTARY_PROFILE")
  echo "==> notarytool auth: keychain profile '$NOTARY_PROFILE'"
fi

# --- 1. Build a fresh RELEASE bundle (unless reusing an existing one) --------
if [ -n "${SKIP_BUILD:-}" ]; then
  echo "==> SKIP_BUILD set — signing the existing bundle at $APP"
  [ -d "$APP" ] || { echo "error: $APP not found; run scripts/make-app.sh release first" >&2; exit 1; }
else
  echo "==> building release bundle via scripts/make-app.sh release"
  scripts/make-app.sh release
fi

# --- 2. Sign INSIDE-OUT: nested daemon Mach-O FIRST, then the bundle --------
# Contents/MacOS/ holds TWO Mach-Os: the main exe 'kod' (signed when the bundle
# is signed) and the siblings 'orchestrator-daemon' and 'kod-bridge' (each is
# resolved next to the current_exe() of whatever starts it; codesign does NOT
# auto-sign them, so sign them explicitly first). An UNSIGNED nested Mach-O is
# SIGKILLed by Gatekeeper with an empty stderr — the least diagnosable failure
# there is, so missing one here is not a cosmetic omission.
# --timestamp is MANDATORY for notarization (secure timestamp). We apply
# signatures explicitly rather than with the deprecated `--deep`.
echo "==> signing nested helper: orchestrator-daemon"
codesign --force --timestamp --options runtime \
  --entitlements "$ENT" --sign "$SIGN_IDENTITY" \
  "$APP/Contents/MacOS/orchestrator-daemon"

echo "==> signing nested helper: kod-bridge (the mobile bridge)"
codesign --force --timestamp --options runtime \
  --entitlements "$ENT" --sign "$SIGN_IDENTITY" \
  "$APP/Contents/MacOS/kod-bridge"

echo "==> signing the bundle (main exe 'kod' + seal)"
codesign --force --timestamp --options runtime \
  --entitlements "$ENT" --sign "$SIGN_IDENTITY" \
  "$APP"

# Local verify before spending a notarization round-trip.
echo "==> local signature verify"
codesign --verify --strict --verbose=4 "$APP"
codesign --display --entitlements - "$APP"

# --- 3. Zip with ditto (preserves bundle symlinks; plain `zip` corrupts it) --
echo "==> packaging $ZIP with ditto"
mkdir -p "$DIST"
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

# --- 4. Submit to notarytool and WAIT for the verdict -----------------------
echo "==> submitting to notarytool (waiting for verdict)"
SUBMIT_RC=0
SUBMIT_OUT="$(xcrun notarytool submit "$ZIP" "${NOTARY_AUTH[@]}" --wait 2>&1)" || SUBMIT_RC=$?
echo "$SUBMIT_OUT"

SUBMISSION_ID="$(printf '%s\n' "$SUBMIT_OUT" | grep -oE '\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b' | head -n1 || true)"
if ! printf '%s\n' "$SUBMIT_OUT" | grep -q 'status: Accepted'; then
  echo "error: notarization did not reach 'Accepted' (rc=$SUBMIT_RC)." >&2
  if [ -n "$SUBMISSION_ID" ]; then
    echo "==> fetching rejection log for submission $SUBMISSION_ID" >&2
    xcrun notarytool log "$SUBMISSION_ID" "${NOTARY_AUTH[@]}" >&2 || true
  fi
  exit 1
fi
echo "==> notarization Accepted (submission $SUBMISSION_ID)"

# --- 5. Staple the ticket to the .app, then verify the whole chain ----------
# The ticket attaches to the BUNDLE (not the zip); stapling makes the app pass
# Gatekeeper offline.
echo "==> stapling + validating"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

echo "==> Gatekeeper assessment (expect: accepted, source=Notarized Developer ID)"
spctl -a -vvv -t exec "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# --- 6. Re-zip the STAPLED app for distribution -----------------------------
echo "==> re-packaging the stapled app -> $ZIP"
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

echo ""
echo "DONE: signed + notarized + stapled"
echo "  app:      $(cd "$(dirname "$APP")" && pwd)/$(basename "$APP")"
echo "  artifact: $(cd "$DIST" && pwd)/$(basename "$ZIP")"
