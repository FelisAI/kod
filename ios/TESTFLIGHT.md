# Shipping Kod Remote to external TestFlight

## Build

    cd ios
    xcodegen generate
    xcodebuild archive -project Kod.xcodeproj -scheme Kod -configuration Release \
      -destination 'generic/platform=iOS' -archivePath build-archive/Kod.xcarchive \
      -allowProvisioningUpdates
    xcodebuild -exportArchive -archivePath build-archive/Kod.xcarchive \
      -exportOptionsPlist export.plist -exportPath build-export -allowProvisioningUpdates

Produces `build-export/Kod.ipa`, signed **Apple Distribution: Felis AI LLC
(BHX233597M)** with a store provisioning profile. `-allowProvisioningUpdates`
creates the distribution certificate on first run, so no manual cert wrangling.

## Upload

Needs credentials this repo does not hold. Either:

    xcrun altool --upload-app -f build-export/Kod.ipa -t ios \
      --apiKey <KEY_ID> --apiIssuer <ISSUER_ID>

with the `.p8` in `~/.appstoreconnect/private_keys/`, or drag the `.ipa` into
Xcode's Organizer. The app record for `pro.felisai.kod.remote` must exist in App
Store Connect first.

## Export compliance

`ITSAppUsesNonExemptEncryption` is `false`. The app uses TLS and SHA-256 through
Apple's own frameworks — standard algorithms, which are exempt. It implements no
cryptography of its own. Answer "No" to the encryption question.

## THE REVIEW RISK, and what to say about it

Kod Remote is a **companion app**: it does nothing without a Mac running Kod on
the same Tailscale network. A reviewer opening it cold sees a connection screen
and no way in. That is the single most likely rejection, under Guideline 2.1 (app
completeness) or 4.2 (minimum functionality), and it is worth pre-empting in the
App Review notes rather than arguing afterwards.

Suggested notes:

> Kod Remote is the companion to Kod, a macOS app for running coding agents
> (claude, codex). It shows the sessions running on the user's own Mac and lets
> them answer an agent that is waiting on a question.
>
> It connects ONLY to a server the user runs themselves, over their own Tailscale
> network or LAN — there is no service of ours involved and no account to create.
> Pairing is by QR code shown in the Mac app, which carries the address, an access
> token, and the fingerprint of the Mac's TLS key; the app pins that key and
> refuses any other. Because of that it cannot be exercised without the desktop
> app and a paired Mac.
>
> A demo video showing the full flow — pairing, reading sessions, answering an
> agent — is attached. We are happy to supply a build of the macOS app and a test
> machine on request.

Attach a screen recording. It is the thing that gets companion apps through.

## What is deliberately NOT in this build

- **iPhone only** (`TARGETED_DEVICE_FAMILY: "1"`). iPad would oblige iPad
  screenshots and a layout the Session reader and composer were not designed for.
- **No spawning.** The phone cannot start a session, open a shell, or close
  anything; it reads, and it types into claude/codex sessions only. The daemon
  enforces that, not the app.
- **ATS exception.** `NSAllowsArbitraryLoads` is still required: the Mac presents
  a self-signed certificate (no CA will issue one for a private IP), so the
  connection cannot chain to a trusted anchor. Identity comes from the pinned key
  instead, which is stronger for this case than CA trust — no CA is trusted at
  all. If review asks, that is the answer. Do NOT add `NSAllowsLocalNetworking`
  alongside it: iOS then ignores `NSAllowsArbitraryLoads` and honours only the
  local-networking exemption, which does not cover a 100.64/10 Tailscale address.
  That combination cost an evening once already.

## External testing specifics

External TestFlight (up to 10,000 testers, public link) needs Beta App Review —
lighter than App Review, but the companion-app problem above still applies.
Builds expire after **90 days**, so a public link means a build treadmill.
Internal testing (up to 100 team members) needs no review at all and is the right
first step.
