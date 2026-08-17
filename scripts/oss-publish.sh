#!/usr/bin/env bash
#
# Clean-room OSS publish.
#
# Builds a FRESH, SINGLE-COMMIT public repository from the current working tree,
# with NO git history and WITHOUT any internal/private content. History is dropped
# on purpose: past commits contain removed personal data, deleted integrations with
# retired internal projects, personal fixtures, etc. — scrubbing the working tree
# does not scrub history,
# so the public repo must start from one commit.
#
# This does NOT touch the private repo and does NOT push anything. It stages a
# ready-to-push repo, RUNS A LEAK GATE over the actual shipped tree, and prints the
# next steps. If the gate finds anything it ABORTS and commits nothing.
#
# Usage:   scripts/oss-publish.sh [OUT_DIR]
#   OUT_DIR  where to build the public repo (default: ../<repo>-public)
#
# Prereqs: run from the repo root, on a clean tree, AFTER the rename + README are done.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUT="${1:-$ROOT/../$(basename "$ROOT")-public}"

# --- what SHIPS (whitelist by top-level path) -------------------------------
#   app/            the product (crates, fixtures, build scripts) — de-personalized
#   .github/        CI
#   LICENSE NOTICE README.md CONTRIBUTING.md .gitignore
# Everything else (designs/, docs/, spikes/, the rest of this scripts/ dir, the
# untracked ecphory/ memory prototype, ...) is EXCLUDED.
#
# The one file-level exception is the THIRD-PARTY-LICENSES generator: that file's
# own header tells a reader to regenerate it with
# `python3 scripts/gen-third-party-licenses.py`, so shipping the attribution
# without the script would publish a dangling instruction. Entries are matched
# against the whole path as well as the top-level component, so naming the file
# ships it without shipping the directory.
INCLUDE_TOP=(app .github LICENSE NOTICE README.md CONTRIBUTING.md CODE_OF_CONDUCT.md SECURITY.md THIRD-PARTY-LICENSES .gitignore scripts/gen-third-party-licenses.py)

# --- what must NEVER ship even though its top-level IS whitelisted -----------
# The memory EVAL HARNESS is private research WIP written around retired project
# names; the product does not need it (the only compile-time tie is a #[cfg(test)]
# fixture-load test that include_str!s eval.json — which we scrub, below, so it
# still builds). Exact git-relative paths.
EXCLUDE_PATHS=(
  "app/crates/orchestrator-store/examples/memory_eval.rs"
  "app/crates/orchestrator-store/examples/memory_shadow_eval.rs"
  "app/fixtures/memory/README.md"
)

# --- the leak gate's vocabulary --------------------------------------------
# Retired/private names + personal identifiers that must appear NOWHERE in the
# shipped tree (content OR filename). Case-insensitive.
#
# LOADED FROM AN UNTRACKED FILE, and that is the point: the vocabulary is a list
# of the exact strings we are trying to keep private — email addresses among them
# — so hardcoding it here published every one of them in the very script meant to
# prevent that. `scripts/leakgate.local` is gitignored; this script is not.
#
# The fallback list below is deliberately EMPTY of personal data. If the local
# file is missing the gate still runs, but only against credential shapes, and it
# says so loudly rather than pretending to be a name gate it is not.
LEAKGATE_LOCAL="$ROOT/scripts/leakgate.local"
if [ -f "$LEAKGATE_LOCAL" ]; then
  # shellcheck disable=SC1090
  . "$LEAKGATE_LOCAL"
  : "${LEAK_RE:?leakgate.local must define LEAK_RE}"
else
  echo "WARNING: $LEAKGATE_LOCAL not found — the NAME gate is disabled." >&2
  echo "         Only credential shapes will be checked. Create it from" >&2
  echo "         scripts/leakgate.local.example before a real publish." >&2
  LEAK_RE='(?!)__no_name_gate_configured__'
fi
# Credential shapes. Conservative — anchored so ordinary prose can't trip them.
SECRET_RE='sk-ant-|sk-[A-Za-z0-9]{20}|ghp_[A-Za-z0-9]|github_pat_|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----'

is_excluded() {
  local f="$1" p
  for p in "${EXCLUDE_PATHS[@]}"; do [ "$f" = "$p" ] && return 0; done
  return 1
}

# --- pre-flight -------------------------------------------------------------
[ -f LICENSE ] || { echo "ERROR: LICENSE missing — add it before publishing." >&2; exit 1; }
[ -f README.md ] || { echo "ERROR: README.md missing — write it (after the rename) before publishing." >&2; exit 1; }

# The tree must be CLEAN, and this is enforced rather than merely documented,
# because the whole script reads from `git ls-files` — the INDEX, not the disk.
# A dirty tree fails in two different ways and only one of them is loud:
#   * a tracked-but-deleted file is still listed, so `cp` dies partway through
#     and leaves a half-built tree (this is the loud one);
#   * an untracked NEW file is not listed at all, so it is silently omitted —
#     the publish "succeeds" and the public repo is missing whatever you just
#     added. That is the dangerous one: a release shipped without its own new
#     assets, with a clean exit code.
# Committing first is the fix, not overriding this.
if [ -z "${OSS_PUBLISH_ALLOW_DIRTY:-}" ] && [ -n "$(git status --porcelain)" ]; then
  echo "ERROR: working tree is dirty — commit (or stash) before publishing." >&2
  echo "  This script copies from \`git ls-files\`, so UNTRACKED files are silently" >&2
  echo "  omitted from the public tree and DELETED-but-tracked files abort the copy." >&2
  echo "" >&2
  git status --short >&2
  echo "" >&2
  echo "  (set OSS_PUBLISH_ALLOW_DIRTY=1 only if you understand both failure modes.)" >&2
  exit 1
fi

# --- build the clean tree (tracked files only, minus the internal dirs) ------
echo ">> building public tree at: $OUT"
rm -rf "$OUT"
mkdir -p "$OUT"

# git ls-files → tracked files only (so the untracked ecphory/ can never sneak in),
# then keep only whitelisted top-level paths, minus the explicit exclusions.
git ls-files | while IFS= read -r f; do
  top="${f%%/*}"
  keep=0
  for w in "${INCLUDE_TOP[@]}"; do
    if [ "$f" = "$w" ] || [ "$top" = "$w" ]; then keep=1; break; fi
  done
  [ "$keep" = 1 ] || continue
  is_excluded "$f" && continue
  mkdir -p "$OUT/$(dirname "$f")"
  case "$f" in
    .gitignore)
      # ship a SANITIZED .gitignore — drop any line that names a private/retired
      # thing (e.g. the `/ecphory/` ignore rule); the untracked dirs it named can
      # never ship anyway (we copy tracked files only).
      grep -viE "$LEAK_RE" "$f" > "$OUT/$f" || true
      ;;
    app/fixtures/memory/orchestrator/eval.json)
      # a #[cfg(test)] test include_str!s this fixture, so it MUST ship to compile —
      # but scrub the retired-name PROSE. The loader only asserts project_key +
      # task-count, both untouched by a token substitution.
      sed -E 's/[Gg]brain/wiki/g; s/[Ee]cphory/memory/g' "$f" > "$OUT/$f"
      ;;
    *)
      cp "$f" "$OUT/$f"
      ;;
  esac
done

# --- LEAK GATE: scan the ACTUAL shipped tree before committing anything ------
echo ">> leak gate: scanning shipped tree…"
fail=0
# -i is LOAD-BEARING, not decoration. LEAK_RE is written lowercase and this gate
# ran case-SENSITIVE while the filename gate below and the .gitignore scrub above
# both used -i — so a name in prose capitalization sailed through the one check
# that reads file CONTENT. It was not hypothetical: app/scripts/sign-notarize.sh
# carried a real name, a private email address and a Team ID through a gate that
# reported the tree clean. Any new grep of LEAK_RE must be -i too.
if grep -rIliE "$LEAK_RE" "$OUT" >/dev/null 2>&1; then
  echo "ERROR: retired/private name in shipped CONTENT:" >&2
  grep -rIniE "$LEAK_RE" "$OUT" 2>/dev/null | sed "s#^$OUT/##" >&2
  fail=1
fi
# match RELATIVE paths only — the OUT dir itself may sit under a path containing a
# flagged token (e.g. the default `/Users/<founder>/…-public`), which is not a leak
# in what SHIPS.
if ( cd "$OUT" && find . -type f | sed 's#^\./##' | grep -iE "$LEAK_RE" ) >/dev/null 2>&1; then
  echo "ERROR: retired/private name in shipped FILENAME:" >&2
  ( cd "$OUT" && find . -type f | sed 's#^\./##' | grep -iE "$LEAK_RE" ) >&2
  fail=1
fi
if grep -rIlE "$SECRET_RE" "$OUT" >/dev/null 2>&1; then
  echo "ERROR: possible CREDENTIAL in shipped tree:" >&2
  grep -rInE "$SECRET_RE" "$OUT" 2>/dev/null | sed "s#^$OUT/##" >&2
  fail=1
fi
# absolute home paths → WARN only (the name gate already hard-fails on the founder's).
if grep -rIlE '/Users/[A-Za-z]' "$OUT" >/dev/null 2>&1; then
  echo "WARNING: absolute /Users/ path(s) in shipped tree — review before pushing:" >&2
  grep -rInE '/Users/[A-Za-z]' "$OUT" 2>/dev/null | sed "s#^$OUT/##" >&2
fi
if [ "$fail" = 1 ]; then
  echo "ABORT: leak gate failed — nothing was committed. Fix the sources above and re-run." >&2
  rm -rf "$OUT"
  exit 1
fi
echo ">> leak gate: CLEAN."

# --- init a fresh single-commit repo ---------------------------------------
cd "$OUT"
git init -q
git add -A
# Author/commit under the PUBLIC GitHub identity Kod is published as — NOT whatever
# global git config this machine carries (a different, private email the leak gate
# scrubs from the tree). `-c` forces author + committer + the `-s` sign-off to this
# identity for THIS commit only. This script is excluded from the published tree,
# so naming the public identity here does not ship.
# Set in scripts/leakgate.local (untracked) or the environment. NO personal
# default here: a name and a private email hardcoded in this script were exactly
# the kind of thing the gate below exists to catch, and it could not catch them
# because this script is excluded from the tree it scans.
PUBLISH_NAME="${GIT_PUBLISH_NAME:?set GIT_PUBLISH_NAME (see scripts/leakgate.local.example)}"
PUBLISH_EMAIL="${GIT_PUBLISH_EMAIL:?set GIT_PUBLISH_EMAIL (see scripts/leakgate.local.example)}"
git -c commit.gpgsign=false \
    -c "user.name=$PUBLISH_NAME" \
    -c "user.email=$PUBLISH_EMAIL" \
    commit -q -s -m "Initial public release"

echo ""
echo ">> done. clean single-commit public repo at: $OUT"
echo "   files: $(git ls-files | wc -l | tr -d ' ')  commit: $(git rev-parse --short HEAD)"
echo ""
echo "Next steps (review, then push to the NEW public remote):"
echo "  cd \"$OUT\""
echo "  git log --stat            # sanity-check: one commit, no internal trees"
echo "  git show --format=fuller -s HEAD   # confirm the author/committer identity you want public"
echo "  git remote add origin git@github.com:FelisAI/kod.git"
echo "  git push -u origin main"
