# Contributing

Thanks for your interest in contributing!

## Getting started

Kod is a native macOS app (Rust + [GPUI](https://github.com/zed-industries/zed)),
macOS only. See the [README](README.md) for what it does and how to build and run
it. Please open an issue to discuss any substantial change before sending a pull
request — it saves everyone a wasted round-trip.

## Building & testing

The Cargo workspace lives under `app/`. The toolchain (a specific nightly, with
`clippy`) is pinned in `app/rust-toolchain.toml` and installed automatically by
`rustup` on first build. Run everything from `app/`:

```sh
cd app

cargo clippy --workspace     # lint (advisory in CI)
cargo test --workspace       # tests — this is what CI gates on
```

This repo is **hand-formatted and does not use `rustfmt`** — please match the
style of the surrounding code rather than running `cargo fmt`, which would reflow
the entire tree.

The `map` and `memory` subsystems are off by default (see the README). They are
gated on `cfg!()` constants rather than `#[cfg]` attributes, so the default build
still compiles and tests them and they cannot silently rot. CI additionally runs
`cargo check` under `--features map`, `--features memory`, and `--features
map,memory`; if you add a real `#[cfg(feature = …)]` block, those runs are what
will catch a typo inside it.

## Reading the code

Many comments cite internal design documents by number — `docs/019`,
`(docs/012)`, `docs/018 §4`. Those are the project's working notes: they are not
published with the source, and you aren't missing anything you need. The
citations stay in on purpose — they record that a non-obvious rule was *decided*
rather than guessed — and the code is written to stand on its own without them.
If a comment only makes sense with the document open, treat that as a bug in the
comment and say so in an issue rather than going looking for the document. Don't
bulk-strip the references either: a handful of `docs/…` strings are fixture data
that tests assert on.

## Pull requests

- Keep each PR focused on one change.
- `cargo test --workspace` must pass from `app/` before you push (running
  `cargo clippy --workspace` too is appreciated — lints are advisory). Don't run
  `cargo fmt` (see the build note above).
- Every commit must be signed off (see DCO below).

## Developer Certificate of Origin (DCO)

All commits must be signed off. By adding a `Signed-off-by:` line you certify the
[Developer Certificate of Origin](https://developercertificate.org/) — that you
wrote the contribution, or otherwise have the right to submit it under the
project's license. Sign off with:

```sh
git commit -s
```

## Licensing of contributions

Kod is licensed under **Apache-2.0** (see [`LICENSE`](LICENSE)). Contributions are
inbound under the **same license**: by opening a pull request you license your
contribution under Apache-2.0, and your DCO sign-off certifies you have the right
to do so. **There is no CLA** — you keep the copyright to your contribution, and
the project stays Apache-2.0.

## Reporting security issues

Please do **not** file security vulnerabilities as public issues. Report them
privately through the repo's GitHub [security advisories](https://github.com/FelisAI/kod/security/advisories/new)
(**Security → Report a vulnerability**) — see `SECURITY.md`.
