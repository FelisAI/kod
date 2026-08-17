#!/usr/bin/env python3
"""Generate THIRD-PARTY-LICENSES from `cargo metadata` plus the license files
already vendored in the local cargo registry.

Runs with nothing but python3 (>= 3.9) and cargo. See the generated file's
header for what it does and does not guarantee.

    python3 scripts/gen-third-party-licenses.py

Exits nonzero if any package's license expression cannot be satisfied from
ACCEPTED below, so this can gate a release.
"""

import collections
import json
import os
import re
import subprocess
import sys
import textwrap
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CARGO_DIR = os.path.join(REPO, "app")
OUT = os.path.join(REPO, "THIRD-PARTY-LICENSES")

# Election policy. When a package offers a choice (`MIT OR Apache-2.0`) we take
# the earliest entry here. Apache-2.0 leads deliberately: its text is
# holder-independent, so electing it keeps the attribution correct even for the
# crates that ship no license file and therefore no copyright line.
ACCEPTED = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "ISC",
    "BSD-3-Clause",
    "BSD-2-Clause",
    "Zlib",
    "0BSD",
    "Unlicense",
    "MIT-0",
    "CC0-1.0",
    "BSL-1.0",
    "Unicode-3.0",
    "MPL-2.0",
    "NCSA",
]
RANK = {name: i for i, name in enumerate(ACCEPTED)}

# Licenses whose terms oblige us to carry the specific copyright line. Apache-2.0
# and the public-domain dedications are absent on purpose: their text is
# holder-independent, so a crate that ships no copyright line still gets a
# complete attribution.
NOTICE_REQUIRED = set(
    ["MIT", "ISC", "BSD-2-Clause", "BSD-3-Clause", "Zlib", "BSL-1.0", "Unicode-3.0", "NCSA"]
)

# Platform whose link graph gets annotated per package. Kod ships macOS only;
# add targets here and the scope column widens with them.
SCOPE_TARGET = "aarch64-apple-darwin"

# Substrings of the letters-only normalization of a license file, chosen to be
# mutually exclusive: `MIT` and `Unicode-3.0` both open "Permission is hereby
# granted, free of charge, to any person obtaining a copy of", and BSL-1.0 does
# too, so each fingerprint reaches past the shared prefix. A file matching more
# than one is reported as composite rather than silently filed under the first.
FINGERPRINTS = [
    ("Apache-2.0", "apachelicenseversion20january2004"),
    ("LLVM-exception", "llvmexceptionstotheapache20license"),
    ("MIT", "obtainingacopyofthissoftwareandassociateddocumentationfiles"),
    ("MIT-0", "mitnoattribution"),
    ("ISC", "foranypurposewithorwithoutfeeisherebygrantedprovidedthattheabovecopyright"),
    ("0BSD", "foranypurposewithorwithoutfeeisherebygrantedthesoftwareisprovided"),
    ("BSD-3-Clause", "neitherthenameof"),
    ("BSD-2-Clause", "redistributionsinbinaryformmustreproducetheabovecopyrightnotice"),
    ("Zlib", "thisnoticemaynotberemovedoralteredfromanysourcedistribution"),
    ("MPL-2.0", "mozillapubliclicenseversion20"),
    ("Unlicense", "thisisfreeandunencumberedsoftwarereleasedintothepublicdomain"),
    ("CC0-1.0", "cc010universal"),
    ("BSL-1.0", "boostsoftwarelicenseversion10"),
    ("Unicode-3.0", "obtainingacopyofdatafilesandanyassociateddocumentation"),
    ("NCSA", "universityofillinoisncsaopensourcelicense"),
    ("GPL-2.0-only", "gnugeneralpubliclicenseversion2june1991"),
    # MPL-2.0's Exhibit B names the LGPL, so this reaches for LGPL's own date line.
    ("LGPL-2.1", "gnulessergeneralpubliclicenseversion21february1999"),
]

LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|unlicen[cs]e|notice)", re.I)
# `©` needs its own branch: it is not a word character, so a trailing \b would
# never fire on "© Kornel Lesiński" and the notice would be dropped.
COPYRIGHT_LINE = re.compile(r"^\s*(//|#|\*|;)*\s*((Copyright|COPYRIGHT|\(c\)|\(C\))\b|©)")
PLACEHOLDER = re.compile(
    r"\[y{2,4}\]|\{y{2,4}\}|\[year\]|<year>|\{year\}|\[name of copyright owner\]"
    r"|\{name of copyright owner\}|\[fullname\]|\[name\]",
    re.I,
)
# Prose from inside a license body that happens to start a wrapped line with the
# word Copyright: the MIT disclaimer's "COPYRIGHT HOLDERS BE LIABLE...", the
# Unicode heading "COPYRIGHT AND PERMISSION NOTICE", CC0's "Copyright and
# Related Rights". Matching on the following word keeps holder-only notices like
# "Copyright (c) Steven Sheldon", which carry no year to test for.
NOTICE_PROSE = re.compile(
    r"^(Copyright|COPYRIGHT)\s+(and|holders?|notice|licen[cs]e|laws?|or|in|to|shall)\b", re.I
)
TITLE_LINE = re.compile(
    r"^\s*[#*=_\-\s]*(the\s+)?(mit(\s+no\s+attribution)?|isc|zlib|bsd[\s\-]*[0234][\s\-]*clause"
    r"|boost\s+software|apache(\s+license)?(\s*,?\s*version\s*2\.0)?|mozilla\s+public|unicode)?"
    r"\s*licen[cs]e(\s*\(?(mit|isc|zlib|bsd\-?[23]\-?clause)\)?)?\s*[#*=_\-\s]*$",
    re.I,
)
ALL_RIGHTS = re.compile(r"^\s*all\s+rights\s+reserved\.?\s*$", re.I)
RULE_LINE = re.compile(r"^\s*[=\-_*~#]{3,}\s*$")

WRAP = 78


def die(msg):
    sys.stderr.write("gen-third-party-licenses: %s\n" % msg)
    sys.exit(2)


def cargo_metadata(extra):
    cmd = ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"] + extra
    proc = subprocess.run(cmd, cwd=CARGO_DIR, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if proc.returncode != 0:
        die("`%s` failed:\n%s" % (" ".join(cmd), proc.stderr.decode("utf-8", "replace")))
    return json.loads(proc.stdout.decode("utf-8"))


def letters(text):
    return re.sub(r"[^a-z0-9]+", "", text.lower())


def classify(text):
    """Return (license_id, matched_ids). license_id is None if nothing matched
    and "COMPOSITE" if the file asserts several licenses at once."""
    norm = letters(text)
    hits = set(name for name, pat in FINGERPRINTS if pat in norm)
    if hits == set(["Apache-2.0", "LLVM-exception"]):
        return "Apache-2.0 WITH LLVM-exception", hits
    if "BSD-3-Clause" in hits:
        hits.discard("BSD-2-Clause")
    if len(hits) == 1:
        return next(iter(hits)), hits
    if not hits:
        return None, hits
    return "COMPOSITE", hits


def body_key(text):
    """Identity of a license text ignoring the parts that vary per crate without
    changing the terms: the copyright block, the title line, decorative rules,
    and http/https drift in the apache.org URLs."""
    kept = []
    for line in text.splitlines():
        if COPYRIGHT_LINE.match(line) or TITLE_LINE.match(line):
            continue
        if ALL_RIGHTS.match(line) or RULE_LINE.match(line):
            continue
        kept.append(line)
    return letters("\n".join(kept)).replace("https", "http")


def copyright_notices(text):
    out = []
    for line in text.splitlines():
        if not COPYRIGHT_LINE.match(line):
            continue
        s = " ".join(line.split()).strip(" *#/;")
        if not s or len(s) > 200:
            continue
        if PLACEHOLDER.search(s) or NOTICE_PROSE.match(s):
            continue
        if s not in out:
            out.append(s)
    return out


def tokenize(expr):
    return re.findall(r"\(|\)|[A-Za-z0-9.+\-]+", expr)


def parse_expression(expr):
    """SPDX subset -> list of alternative license-id sets (frozensets).

    Slash forms (`MIT/Apache-2.0`) predate SPDX and are not valid expressions;
    cargo still carries dozens of them, so they are rewritten to OR first.
    """
    toks = tokenize(re.sub(r"\s*/\s*", " OR ", expr))
    pos = [0]

    def peek():
        return toks[pos[0]] if pos[0] < len(toks) else None

    def take():
        tok = peek()
        pos[0] += 1
        return tok

    def term():
        tok = take()
        if tok == "(":
            inner = or_expr()
            if peek() != ")":
                raise ValueError("unbalanced parentheses in %r" % expr)
            take()
            return inner
        if tok is None or tok in ("AND", "OR", ")"):
            raise ValueError("unexpected %r in %r" % (tok, expr))
        name = tok
        if peek() == "WITH":
            take()
            exc = take()
            if exc is None:
                raise ValueError("dangling WITH in %r" % expr)
            name = "%s WITH %s" % (name, exc)
        return [frozenset([name])]

    def and_expr():
        alts = term()
        while peek() == "AND":
            take()
            rhs = term()
            alts = [a | b for a in alts for b in rhs]
        return alts

    def or_expr():
        alts = and_expr()
        while peek() == "OR":
            take()
            alts = alts + and_expr()
        return alts

    alts = or_expr()
    if peek() is not None:
        raise ValueError("trailing %r in %r" % (peek(), expr))
    # Order-preserving dedup so `MIT OR MIT` does not read as a choice.
    seen, out = set(), []
    for a in alts:
        if a not in seen:
            seen.add(a)
            out.append(a)
    return out


def elect(alternatives):
    """Pick the alternative to rely on. Returns (chosen, rejected_alternatives)."""
    ranked = []
    for idx, alt in enumerate(alternatives):
        if all(lic in RANK for lic in alt):
            ranked.append((tuple(sorted(RANK[lic] for lic in alt)), idx, alt))
    if not ranked:
        return None, alternatives
    ranked.sort()
    chosen = ranked[0][2]
    return chosen, [a for a in alternatives if a != chosen]


def reachability(meta):
    """Tag every package in `meta` as linked / build / dev, following cargo's own
    dependency kinds out of the workspace members."""
    nodes = dict((n["id"], n) for n in meta["resolve"]["nodes"])
    members = list(meta["workspace_members"])

    def closure(seed):
        seen, stack = set(), list(seed)
        while stack:
            cur = stack.pop()
            if cur in seen or cur not in nodes:
                continue
            seen.add(cur)
            for dep in nodes[cur]["deps"]:
                if any(k.get("kind") is None for k in dep["dep_kinds"]):
                    stack.append(dep["pkg"])
        return seen

    linked = closure(members)

    def seeds_of(kind):
        out = []
        for nid in linked:
            for dep in nodes[nid]["deps"]:
                if any(k.get("kind") == kind for k in dep["dep_kinds"]):
                    out.append(dep["pkg"])
        return out

    build = closure(seeds_of("build")) - linked
    dev = closure(seeds_of("dev")) - linked - build
    tags = {}
    for nid in linked:
        tags[nid] = "linked"
    for nid in build:
        tags[nid] = "build"
    for nid in dev:
        tags[nid] = "dev"
    return tags


def source_label(pkg):
    src = pkg.get("source")
    if src is None:
        return "local path (not redistributed)"
    if src.startswith("registry+https://github.com/rust-lang/crates.io-index"):
        return "crates.io"
    if src.startswith("git+"):
        return "git: %s" % src[4:]
    return src


def read_license_files(pkg):
    """Every license-ish file in the package's own directory. Symlinks are read
    through: the alacritty fork ships alacritty_terminal/LICENSE-APACHE as a link
    to the workspace root copy."""
    directory = os.path.dirname(pkg["manifest_path"])
    out = []
    try:
        names = sorted(os.listdir(directory))
    except OSError:
        return out
    for name in names:
        path = os.path.join(directory, name)
        if not LICENSE_FILE.match(name) or not os.path.isfile(path):
            continue
        try:
            with open(path, "rb") as fh:
                raw = fh.read()
        except OSError:
            continue
        out.append((name, raw.decode("utf-8", "replace")))
    return out


# Directories that hold the crate's own code rather than anything vendored, and
# so cannot carry a third party's license.
VENDOR_SKIP_DIRS = set(
    ["src", "test", "tests", "bench", "benches", "example", "examples", "target",
     "doc", "docs", "fuzz"]
)
VENDOR_MAX_DEPTH = 3


def read_vendored_files(pkg):
    """License files below the package root — code vendored into a crate under
    its own terms, such as ring's third_party/fiat or freetype-sys's freetype2.
    Invisible to a top-level scan, and the crate's own SPDX expression does not
    cover them."""
    root = os.path.dirname(pkg["manifest_path"])
    out = []
    for current, dirs, files in os.walk(root):
        rel = os.path.relpath(current, root)
        depth = 0 if rel == "." else rel.count(os.sep) + 1
        if depth >= VENDOR_MAX_DEPTH:
            dirs[:] = []
        else:
            dirs[:] = sorted(d for d in dirs if d not in VENDOR_SKIP_DIRS and not d.startswith("."))
        if depth == 0:
            continue
        for name in sorted(files):
            if not LICENSE_FILE.match(name):
                continue
            path = os.path.join(current, name)
            if not os.path.isfile(path):
                continue
            try:
                with open(path, "rb") as fh:
                    raw = fh.read()
            except OSError:
                continue
            out.append(("%s/%s" % (rel.replace(os.sep, "/"), name), raw.decode("utf-8", "replace")))
    return out


def main():
    meta = cargo_metadata([])
    scope_meta = cargo_metadata(["--filter-platform", SCOPE_TARGET])
    scope_tags = reachability(scope_meta)

    members = set(meta["workspace_members"])
    packages = sorted(
        (p for p in meta["packages"] if p["id"] not in members),
        key=lambda p: (p["name"].lower(), p["name"], p["version"]),
    )
    if not packages:
        die("cargo metadata returned no third-party packages")

    # --- gather license texts -------------------------------------------------
    # variants[license_id][body_key] = list of (crate, version, filename, text)
    variants = collections.defaultdict(lambda: collections.defaultdict(list))
    composite = []
    notice_files = collections.defaultdict(list)  # text -> [(crate, version, filename)]
    stub_files = []
    vendored = []
    per_pkg = {}

    for pkg in packages:
        key = (pkg["name"], pkg["version"])
        info = {"licenses": [], "notices": [], "files": []}
        for name, text in read_license_files(pkg):
            lic, hits = classify(text)
            info["files"].append((name, lic))
            if lic == "COMPOSITE":
                composite.append((pkg["name"], pkg["version"], name, sorted(hits), text))
                info["notices"] += copyright_notices(text)
                continue
            if lic is None:
                stripped = text.strip()
                if len(stripped) < 64 and "\n" not in stripped:
                    stub_files.append((pkg["name"], pkg["version"], name, stripped))
                else:
                    notice_files[text].append((pkg["name"], pkg["version"], name))
                info["notices"] += copyright_notices(text)
                continue
            variants[lic][body_key(text)].append((pkg["name"], pkg["version"], name, text))
            info["licenses"].append(lic)
            if lic not in ("GPL-2.0-only", "LGPL-2.1"):
                info["notices"] += copyright_notices(text)
        for name, text in read_vendored_files(pkg):
            lic, hits = classify(text)
            vendored.append((pkg["name"], pkg["version"], name, lic, sorted(hits), text))
        deduped = []
        for n in info["notices"]:
            if n not in deduped:
                deduped.append(n)
        info["notices"] = deduped
        per_pkg[key] = info

    # --- elect a license per package ------------------------------------------
    unsatisfiable = []
    no_license_field = []
    license_file_only = []
    had_denied_alternative = []
    conjunctive = []
    non_registry = []
    text_missing = []
    no_notice = []
    elected_ids = set()

    for pkg in packages:
        key = (pkg["name"], pkg["version"])
        info = per_pkg[key]
        expr = pkg.get("license")
        info["expr"] = expr
        info["scope"] = scope_tags.get(pkg["id"], "not built for %s" % SCOPE_TARGET)
        src = pkg.get("source")
        if src is None or not src.startswith("registry+"):
            non_registry.append(pkg)
        if not expr:
            no_license_field.append(pkg)
            if pkg.get("license_file"):
                license_file_only.append(pkg)
            info["chosen"] = None
            info["choice"] = False
            continue
        try:
            alts = parse_expression(expr)
        except ValueError as exc:
            die("cannot parse license expression for %s %s: %s" % (pkg["name"], pkg["version"], exc))
        chosen, rejected = elect(alts)
        if chosen is None:
            unsatisfiable.append((pkg, expr))
            info["chosen"] = None
            info["choice"] = len(alts) > 1
            continue
        info["chosen"] = sorted(chosen)
        info["choice"] = len(alts) > 1
        elected_ids |= set(chosen)
        if len(chosen) > 1:
            conjunctive.append((pkg, expr, sorted(chosen)))
        if any(any(lic not in RANK for lic in alt) for alt in rejected):
            denied = sorted(set(lic for alt in rejected for lic in alt if lic not in RANK))
            had_denied_alternative.append((pkg, expr, sorted(chosen), denied))
        if not info["files"]:
            text_missing.append((pkg, expr, sorted(chosen)))
        if (set(chosen) & NOTICE_REQUIRED) and not info["notices"]:
            no_notice.append((pkg, expr, sorted(chosen)))

    if unsatisfiable:
        for pkg, expr in unsatisfiable:
            sys.stderr.write(
                "  %s %s: %s offers no alternative on the accept list\n"
                % (pkg["name"], pkg["version"], expr)
            )
        die("%d package(s) cannot be satisfied from ACCEPTED" % len(unsatisfiable))

    # --- pick one canonical text per elected license --------------------------
    canonical = {}
    divergent = collections.defaultdict(list)
    for lic in sorted(elected_ids):
        groups = variants.get(lic)
        if not groups:
            continue
        ordered = sorted(
            groups.items(),
            key=lambda kv: (-len(kv[1]), sorted(kv[1])[0][0], sorted(kv[1])[0][1]),
        )
        head = sorted(ordered[0][1])[0]
        identical = set()
        for _, group in groups.items():
            for crate, version, _fname, text in group:
                if text == head[3]:
                    identical.add((crate, version))
        equivalent = set(
            (crate, version) for crate, version, _f, _t in ordered[0][1]
        ) - identical
        canonical[lic] = {
            "text": head[3],
            "from": head,
            "identical": len(identical),
            "equivalent": len(equivalent),
        }
        for _, group in ordered[1:]:
            for crate, version, fname, _text in sorted(group):
                divergent[lic].append((crate, version, fname))

    missing_text = sorted(lic for lic in elected_ids if lic not in canonical)

    stamp = time.gmtime(int(os.environ.get("SOURCE_DATE_EPOCH", time.time())))
    date = time.strftime("%Y-%m-%d", stamp)

    expr_counts = collections.Counter(p.get("license") or "(none declared)" for p in packages)
    scope_counts = collections.Counter(per_pkg[(p["name"], p["version"])]["scope"] for p in packages)

    out = []
    w = out.append

    def para(text):
        w(textwrap.fill(" ".join(text.split()), width=WRAP))
        w("")

    # --- header ---------------------------------------------------------------
    w("Third-Party Licenses — Kod")
    w("Copyright 2026 FelisAI")
    w("")
    para(
        "Kod (github.com/FelisAI/kod) is licensed under the Apache License, Version"
        " 2.0 (see LICENSE and NOTICE). It builds on third-party open-source"
        " components, each redistributed under its own license. This file attributes"
        " all of them."
    )
    w("THIS FILE IS GENERATED — DO NOT EDIT BY HAND. Regenerate with:")
    w("")
    w("    python3 scripts/gen-third-party-licenses.py")
    w("")
    para(
        "Generated %s from `cargo metadata --all-features --locked` over"
        " app/Cargo.lock, plus the license files shipped in each crate's own source"
        " distribution. The generator needs only python3 and cargo; it installs"
        " nothing and reaches no network." % date
    )
    para(
        "Scope. The attribution obligations recorded here are those of the code Kod"
        " DISTRIBUTES: the packages that link into an %s binary. Every package in the"
        " resolved dependency graph is still listed — %d of them, across all"
        " platforms and all cargo features, excluding only the workspace's own"
        " orchestrator-* crates — and each carries its role (%s), so nothing is"
        " hidden by omission. But a package tagged build-only or not-built-for-%s is"
        " never present in a shipped artifact, and this file does not claim to"
        " discharge licence obligations for code that is not distributed."
        % (
            SCOPE_TARGET,
            len(packages),
            ", ".join("%s: %d" % (k, v) for k, v in sorted(scope_counts.items())),
            SCOPE_TARGET,
        )
    )
    # The one place that scoping decision has teeth, stated by name rather than
    # left for a reader to notice: libfuzzer-sys declares `(MIT OR Apache-2.0) AND
    # NCSA`, and the AND makes NCSA mandatory rather than electable — but no NCSA
    # text exists anywhere in this dependency tree to reproduce. It is a dev/fuzzing
    # dependency that never reaches a shipped binary, so no obligation attaches.
    # Naming it is the difference between a scoping decision and a silent gap.
    unshipped_and = [
        p
        for p in packages
        if per_pkg[(p["name"], p["version"])]["scope"] != "linked"
        and " AND " in (p.get("license") or "")
    ]
    if unshipped_and:
        para(
            "One consequence worth naming: %s declares a conjunctive licence whose"
            " every term would have to be reproduced if the package shipped. %s not"
            " built for %s, so no attribution obligation attaches and no text for"
            " those terms appears below. This is the only place the scope above"
            " changes what gets printed."
            % (
                ", ".join(
                    "%s %s (%s)" % (p["name"], p["version"], p["license"])
                    for p in unshipped_and
                ),
                "It is" if len(unshipped_and) == 1 else "They are",
                SCOPE_TARGET,
            )
        )
    para(
        "License election. Where a package offers a choice (`MIT OR Apache-2.0`),"
        " this file records the one branch Kod relies on, chosen by a fixed priority"
        " list with Apache-2.0 first — its text is holder-independent, so the"
        " attribution stays correct even for crates that ship no copyright line."
        " Conjunctive expressions (`Apache-2.0 AND ISC`) are never reduced: every"
        " term is recorded and its text reproduced. The generator exits nonzero if"
        " any package offers no branch on that list, so an accidental copyleft"
        " dependency fails the build rather than landing here quietly."
    )
    para(
        "Full license texts appear once each at the bottom, under LICENSE TEXTS,"
        " rather than being repeated per package. Per-package copyright notices are"
        " NOT collapsed — MIT, BSD, ISC and Zlib each require the specific notice to"
        " travel with the software, so every notice found is reproduced under its own"
        " package."
    )
    para(
        "What this generator does not do, and what still needs a human. It identifies"
        " a license file by fingerprinting distinctive phrases of its text, not by"
        " name and not with a statistical matcher, so a file whose terms were edited"
        " around those phrases would still be recognised as the stock license. It"
        " finds code vendored inside a crate only when that code carries a license"
        " file of its own, so a public-domain amalgamation compiled into a -sys crate"
        " leaves no trace here. It has no bundled SPDX"
        " corpus: a license nobody in the graph ships text for cannot be synthesized,"
        " and is named instead under \"Licenses relied on with no text available"
        " locally\" below. It reproduces one canonical copy per license and names"
        " every crate whose own copy differs. For a tagged public release,"
        " `cargo about` — which does carry an SPDX corpus and the askalono matcher —"
        " remains the stronger check. But this file no longer depends on that being"
        " run: every case the generator could not fully resolve is named in the"
        " sections below rather than omitted."
    )
    w("")
    w("=" * WRAP)
    w("SUMMARY")
    w("=" * WRAP)
    w("")
    for label, value in [
        ("third-party packages", len(packages)),
        ("distinct license expressions", len(expr_counts)),
        ("licenses relied on", len(elected_ids)),
        ("license texts reproduced", len(canonical)),
        ("packages with no license declared", len(no_license_field)),
        ("packages not from crates.io", len(non_registry)),
        ("packages shipping no license file", len(text_missing)),
        ("packages with no copyright notice", len(no_notice)),
        ("vendored subtrees with own license", len(vendored)),
    ]:
        w("  %-38s %d" % (label, value))
    w("")

    w("")
    w("=" * WRAP)
    w("LICENSE EXPRESSIONS AS DECLARED")
    w("=" * WRAP)
    w("")
    w("Verbatim from each package's Cargo.toml, before any normalization. Slash forms")
    w("(`MIT/Apache-2.0`) are not valid SPDX; they are read as OR.")
    w("")
    for expr, count in sorted(expr_counts.items(), key=lambda kv: (-kv[1], kv[0])):
        w("  %4d  %s" % (count, expr))
    w("")

    # --- attention sections ---------------------------------------------------
    w("")
    w("=" * WRAP)
    w("PACKAGES NEEDING EXPLICIT ATTENTION")
    w("=" * WRAP)
    w("")

    w("-- No license declared in Cargo.toml --")
    w("")
    if no_license_field:
        w("These packages declare no `license` field. Their terms were NOT determined")
        w("by this generator and must be resolved by hand before release:")
        w("")
        for pkg in no_license_field:
            w("  %s %s   license_file=%s" % (pkg["name"], pkg["version"], pkg.get("license_file")))
    else:
        w("None. Every one of the %d packages declares a machine-readable license" % len(packages))
        w("expression.")
    w("")

    w("-- License given only as a bundled file, with no SPDX expression --")
    w("")
    if license_file_only:
        for pkg in license_file_only:
            w("  %s %s   %s" % (pkg["name"], pkg["version"], pkg.get("license_file")))
    else:
        w("None. No package in the graph uses Cargo's `license-file` escape hatch.")
    w("")

    w("-- Packages not sourced from crates.io --")
    w("")
    if non_registry:
        for pkg in non_registry:
            w("  %s %s" % (pkg["name"], pkg["version"]))
            w("      %s" % source_label(pkg))
            w("      declared license: %s" % (pkg.get("license") or "(none)"))
            files = [n for n, _ in per_pkg[(pkg["name"], pkg["version"])]["files"]]
            w("      license files in checkout: %s" % (", ".join(files) if files else "(none)"))
    else:
        w("None.")
    w("")

    w("-- Expressions that also offered a license Kod does not accept --")
    w("")
    if had_denied_alternative:
        w("Each of these is dual-licensed with a copyleft alternative. Kod elects the")
        w("permissive branch shown; the rejected branch is recorded so the choice is")
        w("auditable rather than implicit.")
        w("")
        for pkg, expr, chosen, denied in had_denied_alternative:
            w("  %s %s" % (pkg["name"], pkg["version"]))
            w("      declared: %s" % expr)
            w("      elected:  %s" % " AND ".join(chosen))
            w("      declined: %s" % ", ".join(denied))
    else:
        w("None.")
    w("")

    w("-- Conjunctive expressions: every term applies at once --")
    w("")
    if conjunctive:
        w("These cannot be reduced to a single license. All terms below are in force")
        w("simultaneously and every one of their texts is reproduced.")
        w("")
        for pkg, expr, chosen in conjunctive:
            w("  %s %s" % (pkg["name"], pkg["version"]))
            w("      declared: %s" % expr)
            w("      in force: %s" % " AND ".join(chosen))
    else:
        w("None.")
    w("")

    w("-- Packages that ship no license file of their own --")
    w("")
    if text_missing:
        w("Upstream declares a license but distributes no copy of it in the crate")
        w("package. The declared terms still govern and the license text appears below;")
        w("what is missing is upstream's own copy, and with it any copyright line. These")
        w("are listed by name rather than dropped:")
        w("")
        for pkg, expr, chosen in text_missing:
            w("  %-40s %-11s %s" % (pkg["name"], pkg["version"], expr))
            repo = pkg.get("repository")
            if repo:
                w("      %s" % repo)
    else:
        w("None.")
    w("")

    w("-- Packages under a notice-requiring license with no copyright line found --")
    w("")
    if no_notice:
        w("MIT, BSD, ISC and Zlib require the copyright notice to be reproduced. For")
        w("these packages no notice could be recovered from the distributed sources, so")
        w("none is claimed. Their attribution is incomplete and the upstream repository")
        w("is the authority:")
        w("")
        for pkg, expr, chosen in no_notice:
            w("  %-40s %-11s %s" % (pkg["name"], pkg["version"], " AND ".join(chosen)))
            repo = pkg.get("repository")
            if repo:
                w("      %s" % repo)
    else:
        w("None.")
    w("")

    w("-- Files asserting more than one license at once --")
    w("")
    if composite:
        w("These files carry several licenses in one document, so none of them was used")
        w("as the canonical text for any single license. They are reproduced in full")
        w("under ADDITIONAL NOTICES.")
        w("")
        for crate, version, fname, hits, _text in sorted(composite):
            w("  %s %s   %s   (%s)" % (crate, version, fname, ", ".join(hits)))
    else:
        w("None.")
    w("")

    w("-- Unresolvable pointer files --")
    w("")
    if stub_files:
        w("Files too short to be license text — upstream packaged a symlink or a bare")
        w("SPDX string instead of the license. The package's declared expression governs;")
        w("the pointer's target is not in the distributed crate.")
        w("")
        for crate, version, fname, content in sorted(stub_files):
            w("  %s %s   %s -> %r" % (crate, version, fname, content))
    else:
        w("None.")
    w("")

    w("-- Licenses relied on with no text available locally --")
    w("")
    if missing_text:
        w("No package in the graph ships a copy of these, and this generator carries no")
        w("license corpus to fall back on. Whether that is a release blocker depends")
        w("entirely on the scope tag beside each package, so it is printed here:")
        w("")
        w("  * a DISTRIBUTED package (linked) with no text IS a blocker — obtain the")
        w("    canonical text from spdx.org and add it before tagging a release.")
        w("  * a package that is build-only or not built for the shipped target is not")
        w("    distributed, so no attribution obligation attaches and nothing is owed.")
        w("")
        for lic in missing_text:
            w("  %s" % lic)
            for p in packages:
                pinfo = per_pkg[(p["name"], p["version"])]
                if pinfo.get("chosen") and lic in pinfo["chosen"]:
                    w("      %s %s [%s]" % (p["name"], p["version"], pinfo["scope"]))
                    if p.get("repository"):
                        w("          %s" % p["repository"])
    else:
        w("None. Every license relied on has its text reproduced below.")
    w("")

    w("-- Crates whose copy of a license differs from the canonical text --")
    w("")
    if divergent:
        w("Their copy is the same license, but its wording is not letter-for-letter")
        w("equivalent to the canonical copy reproduced below — most often because the")
        w("optional Apache appendix is present or absent, or because upstream wrapped")
        w("extra attribution around the license. Only the canonical copy is reproduced")
        w("here; theirs stays in the crate's own source distribution.")
        for lic in sorted(divergent):
            w("")
            w("  %s (%d)" % (lic, len(divergent[lic])))
            for crate, version, fname in divergent[lic]:
                w("      %s %s (%s)" % (crate, version, fname))
    else:
        w("None.")
    w("")

    # --- package list ---------------------------------------------------------
    w("")
    w("=" * WRAP)
    w("PACKAGES (%d)" % len(packages))
    w("=" * WRAP)
    w("")
    w("  name version — declared license [role in an %s build]" % SCOPE_TARGET)
    w("      elected branch, when the declaration offered a choice")
    w("      repository")
    w("      copyright notices found in the package's own license files")
    w("")
    for pkg in packages:
        key = (pkg["name"], pkg["version"])
        info = per_pkg[key]
        w("  %s %s — %s [%s]" % (pkg["name"], pkg["version"], info["expr"] or "(none declared)", info["scope"]))
        if info["choice"] and info["chosen"]:
            w("      elected: %s" % " AND ".join(info["chosen"]))
        src = source_label(pkg)
        if src != "crates.io":
            w("      source: %s" % src)
        w("      %s" % (pkg.get("repository") or "(no repository URL declared)"))
        for notice in info["notices"]:
            w("      %s" % notice)
    w("")

    # --- license texts --------------------------------------------------------
    w("")
    w("=" * WRAP)
    w("LICENSE TEXTS (%d)" % len(canonical))
    w("=" * WRAP)
    w("")
    w("One canonical copy of each license Kod relies on. Every package above is")
    w("covered by the text of the license named in its entry.")
    for lic in sorted(canonical):
        entry = canonical[lic]
        crate, version, fname, text = entry["from"]
        users = [
            p for p in packages
            if per_pkg[(p["name"], p["version"])].get("chosen")
            and lic in per_pkg[(p["name"], p["version"])]["chosen"]
        ]
        w("")
        w("")
        w("-" * WRAP)
        w("%s — applies to %d package(s)" % (lic, len(users)))
        w("-" * WRAP)
        w("")
        w(
            textwrap.fill(
                "Reproduced from %s %s (%s), indented four spaces and stripped of"
                " trailing whitespace, otherwise unaltered. %d package(s) in this graph"
                " ship a byte-identical copy; %d more ship one carrying the same words in the"
                " same order, differing only in line wrapping, indentation,"
                " punctuation, title or copyright lines, or http vs https in a URL."
                " That equivalence is what the deduplication behind this section tests:"
                " letter for letter, ignoring case and every non-alphanumeric character."
                % (crate, version, fname, entry["identical"], entry["equivalent"]),
                width=WRAP,
            )
        )
        w("")
        for line in text.rstrip("\n").splitlines():
            w(("    " + line).rstrip())
    w("")

    # --- extra notices --------------------------------------------------------
    extra = []
    for text, owners in notice_files.items():
        extra.append((sorted(owners), text))
    for crate, version, fname, hits, text in composite:
        extra.append(([(crate, version, fname)], text))
    extra.sort(key=lambda item: (item[0][0][0].lower(), item[0][0][1], item[0][0][2]))

    w("")
    w("=" * WRAP)
    w("ADDITIONAL NOTICES (%d)" % len(extra))
    w("=" * WRAP)
    w("")
    w("Files shipped alongside the licenses above — copyright rosters, per-crate")
    w("licensing statements, and the multi-license documents listed earlier —")
    w("reproduced with the same indent-and-trim treatment as the texts above, and")
    w("otherwise unaltered. Identical files shared by several crates appear once.")
    for owners, text in extra:
        w("")
        w("")
        w("-" * WRAP)
        for crate, version, fname in owners:
            w("%s %s — %s" % (crate, version, fname))
        w("-" * WRAP)
        w("")
        for line in text.rstrip("\n").splitlines():
            w(("    " + line).rstrip())
    w("")

    w("")
    w("=" * WRAP)
    w("VENDORED SUBTREES (%d)" % len(vendored))
    w("=" * WRAP)
    w("")
    if vendored:
        para(
            "Third-party code copied into a crate's own source tree under separate"
            " terms — the crate's SPDX expression does not cover it, and it does not"
            " appear anywhere above. Found by walking up to %d directory levels below"
            " each package root. Code vendored without a license file of its own"
            " (a public-domain amalgamation, say) cannot be found this way and is not"
            " represented here." % VENDOR_MAX_DEPTH
        )
        for crate, version, path, lic, hits, _text in sorted(vendored):
            if lic and lic != "COMPOSITE":
                what = lic
            elif hits:
                what = "several licenses in one file: %s" % ", ".join(hits)
            else:
                # A grant notice or a pointer to texts kept elsewhere in the
                # upstream tree, not a license body — nothing to fingerprint.
                what = "a notice rather than license text"
            w("  %s %s [%s]" % (crate, version, per_pkg[(crate, version)]["scope"]))
            w("      %s — %s" % (path, what))
        for crate, version, path, lic, _hits, text in sorted(vendored):
            w("")
            w("")
            w("-" * WRAP)
            w("%s %s — %s" % (crate, version, path))
            w("-" * WRAP)
            w("")
            for line in text.rstrip("\n").splitlines():
                w(("    " + line).rstrip())
    else:
        w("None.")
    w("")

    with open(OUT, "w") as fh:
        fh.write("\n".join(out).rstrip("\n") + "\n")

    sys.stderr.write(
        "wrote %s: %d packages, %d declared expressions, %d licenses relied on, "
        "%d texts, %d additional notices\n"
        % (os.path.relpath(OUT, REPO), len(packages), len(expr_counts), len(elected_ids),
           len(canonical), len(extra))
    )
    sys.stderr.write("  licenses relied on: %s\n" % ", ".join(sorted(elected_ids)))


if __name__ == "__main__":
    main()
