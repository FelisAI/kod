# Shipping Kod Remote (TestFlight and the App Store)

1.0 (build 2) was submitted to App Review on 2026-10-08 and releases
**automatically** on approval. It went out with Kod 0.4.6, the Mac release whose
bridge sends a phone the terminal of a session sitting still. Build 1 (August)
went through Beta App Review for external TestFlight. The App Store Connect record is "Kod Remote",
`pro.felisai.kod.remote`, Apple ID 6805673576.

## Build and upload

Bump `CURRENT_PROJECT_VERSION` in `project.yml` first — App Store Connect refuses
a build number it has seen. Then:

    cd ios
    xcodegen generate
    xcodebuild archive -project Kod.xcodeproj -scheme Kod -configuration Release \
      -destination 'generic/platform=iOS' -archivePath build-archive/Kod.xcarchive \
      -allowProvisioningUpdates
    xcodebuild -exportArchive -archivePath build-archive/Kod.xcarchive \
      -exportOptionsPlist upload.plist -exportPath build-export -allowProvisioningUpdates

where `upload.plist` is `export.plist` with `destination` set to `upload`. That
uploads through the Apple account signed in to Xcode — no API key needed — and
signs with the cloud-managed Apple Distribution certificate. `export.plist` as
committed writes an `.ipa` instead. Processing takes a few minutes; the build then
reaches the internal TestFlight group on its own.

Check the RELEASE build compiles before archiving
(`xcodebuild build -configuration Release …`): DEBUG-only code once leaked into a
path Release compiles, and only the archive noticed.

## Screenshots

App Store Connect asks for 1206 × 2622 (iPhone with Dynamic Island, medium
display) — the iPhone 17 Pro simulator's exact size. They are generated, not
staged by hand:

    TEST_RUNNER_KOD_SCREENSHOTS=1 xcodebuild test -project Kod.xcodeproj \
      -scheme KodUITests -only-testing:KodUITests/AppStoreScreenshots \
      -destination 'platform=iOS Simulator,name=iPhone 17 Pro' -resultBundlePath shots.xcresult
    xcrun xcresulttool export attachments --path shots.xcresult --output-path shots/

They run the DEBUG sample data with `-kod-screenshots`, which hides the
"sample data" banner. Order on the product page: Standup, a waiting session with
its terminal, the answered session, Projects.

## Export compliance

`ITSAppUsesNonExemptEncryption` is `false`. The app uses TLS and SHA-256 through
Apple's own frameworks — standard algorithms, which are exempt. It implements no
cryptography of its own.

## App Review — the companion-app problem, and the answer to it

Kod Remote does nothing on its own: it needs a Mac running Kod. A reviewer
opening it cold used to see a connection screen and no way in — the classic 2.1 /
4.2 rejection. **"Explore with sample data"** (home screen and connection sheet)
now runs the whole app on built-in sessions, typing included, behind a banner that
says what it is. The review notes lead with it, then explain pairing with a real
Mac, and why `NSAllowsArbitraryLoads` is set: the Mac's certificate is self-signed
for a private IP address, so trust is the pinned public key from the pairing code
— exactly one key accepted, and plaintext refused off the phone's own loopback.
Do NOT add `NSAllowsLocalNetworking` beside it: iOS then ignores
`NSAllowsArbitraryLoads`, and the local-networking exemption does not cover a
100.64/10 Tailscale address.

App Privacy is **Data Not Collected**; the policy is
https://kod.felisai.pro/privacy and support is https://kod.felisai.pro/support
(both in `site/`). Age rating 4+. Free, all regions; not offered on Apple Silicon
Macs or Vision Pro.

## What is deliberately NOT in this build

- **iPhone only** (`TARGETED_DEVICE_FAMILY: "1"`). iPad would oblige iPad
  screenshots and a layout the reader and composer were not designed for.
- **No spawning or closing.** The phone reads, types into live sessions and
  presses the twenty named keys; the daemon enforces that, not the app.

## External testing

External TestFlight needs Beta App Review; builds expire after 90 days, so a
public link means a build treadmill. Internal testing (the "Kod Early Users"
group) needs no review.
