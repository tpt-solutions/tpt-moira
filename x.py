#!/usr/bin/env python3
"""Moira bootstrap orchestrator (the "x.py" of TPT Moira).

Phase 4 / Ouroboros track. This script is the single entry point that wires
the three Fates (clotho, atropos, lachesis) into a self-build pipeline, in the
spirit of rust-lang's `x.py`.

Stages
------
Stage 0 (host build):  build the Moira compiler itself with the *host*
                       toolchain (cargo/rustc). The result is a working
                       `moira` binary that we will later try to use to compile
                       Moira's own source.
Stage 1 (self-host):   use the Stage-0 `moira` to type-check / lower / codegen
                       the `clotho`, `atropos`, and `lachesis` crates. Today
                       Moira only accepts the strict Phase-1 subset, so this
                       stage is a *readiness probe*: it records, per crate, how
                       far Moira gets (parse / check / codegen) so we can track
                       progress toward true self-hosting. When Moira reaches
                       full-subset coverage the probe flips to actually
                       emitting objects and linking the crates.

Subcommands
-----------
  build       cargo build of the workspace (the Stage-0 host compiler).
  check       cargo check of the workspace.
  test        cargo test of the workspace.
  self-host   run the Stage-1 self-host readiness probe (builds stage0 first).
  dist        build release binaries, write checksums, and (with --sign)
              produce detached GPG signatures; stage a distributable tree.
  changelog   generate a changelog from git history since the last tag.
  determinism run the Stage-2 cross-process MIR/SSA/C/ASM parity check.
  all         build + check + test (default).

The self-host probe never fails the build: it always produces a report under
`build/self_host_report.md` and exits 0 so it can run in CI as a tracking job.
When real self-hosting is achievable, set MOIRA_SELFHOST_STRICT=1 to make the
probe fail on any crate that does not fully self-host.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

ROOT = Path(__file__).resolve().parent
BUILD_DIR = ROOT / "build"
REPORT_MD = BUILD_DIR / "self_host_report.md"
REPORT_JSON = BUILD_DIR / "self_host_report.json"

# Crates that make up the compiler proper, in dependency order. The
# self-host stage must compile them in exactly this order because each depends
# on the ones before it.
CRATES: list[dict] = [
    {
        "name": "moira-alloc",
        "path": "tpt-moira-alloc",
        "lib": "tpt-moira-alloc/src/lib.rs",
        "deps": [],
    },
    {
        "name": "clotho",
        "path": "tpt-moira-clotho",
        "lib": "tpt-moira-clotho/src/lib.rs",
        "deps": ["moira-alloc"],
    },
    {
        "name": "atropos",
        "path": "tpt-moira-atropos",
        "lib": "tpt-moira-atropos/src/lib.rs",
        "deps": ["clotho", "moira-alloc"],
    },
    {
        "name": "lachesis",
        "path": "tpt-moira-lachesis",
        "lib": "tpt-moira-lachesis/src/lib.rs",
        "deps": ["clotho", "atropos", "moira-alloc"],
    },
    {
        "name": "moira (driver)",
        "path": ".",
        "lib": "src/main.rs",
        "deps": ["clotho", "lachesis", "atropos"],
    },
]


@dataclass
class ProbeResult:
    crate: str
    lib: str
    parse_ok: bool = False
    check_ok: bool = False
    codegen_ok: bool = False
    notes: list[str] = field(default_factory=list)

    @property
    def self_hostable(self) -> bool:
        return self.parse_ok and self.check_ok and self.codegen_ok


def log(msg: str) -> None:
    print(f"[x.py] {msg}", flush=True)


def run(
    cmd: list[str],
    cwd: Optional[Path] = None,
    check: bool = False,
    env: Optional[dict] = None,
) -> subprocess.CompletedProcess:
    log(" ".join(str(c) for c in cmd))
    return subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        capture_output=True,
        text=True,
        env=env,
    )


def find_moira_binary(release: bool) -> Optional[Path]:
    profile = "release" if release else "debug"
    candidate = ROOT / "target" / profile / ("moira.exe" if os.name == "nt" else "moira")
    return candidate if candidate.exists() else None


def build_host(release: bool) -> Optional[Path]:
    """Stage 0: build the Moira compiler with the host toolchain."""
    profile = ["--release"] if release else []
    log("Stage 0: building host compiler with cargo")
    res = run(["cargo", "build", "--bin", "moira", *profile], cwd=ROOT)
    if res.returncode != 0:
        sys.stderr.write(res.stdout + res.stderr)
        log("host build failed")
        return None
    return find_moira_binary(release)


def probe_crate(moira: Path, crate: dict) -> ProbeResult:
    """Run Moira's pipeline on a crate's entry point and record how far it gets."""
    lib = ROOT / crate["lib"]
    result = ProbeResult(crate=crate["name"], lib=crate["lib"])
    if not lib.exists():
        result.notes.append(f"entry point {crate['lib']} not found")
        return result

    # Cross-crate signature resolution: every *other* workspace crate is
    # passed as `--extern name=lib.rs` so paths like
    # `tpt_moira_atropos::lower_program(...)` resolve during checking.
    extern_args: list[str] = []
    for other in CRATES:
        if other["name"] == crate["name"]:
            continue
        other_lib = ROOT / other["lib"]
        if other_lib.exists():
            extern_args += ["--extern", f"{other['name']}={other_lib}"]

    # Parse stage: `--emit ast` only parses + macro-expands (it never runs the
    # Lachesis checker or the Atropos lowering), so a clean exit is a *pure* parse
    # signal. This lets the probe track parse pass/fail independently of type/borrow
    # checking (TODO: "Re-run and track crate-by-crate parse pass/fail").
    res_parse = run([str(moira), "--emit", "ast", str(lib)])
    result.parse_ok = res_parse.returncode == 0
    if not result.parse_ok:
        err = (res_parse.stderr or res_parse.stdout).strip().splitlines()
        snippet = err[-1] if err else "(no diagnostic)"
        result.notes.append(f"parse failed: {snippet}")
        # A parse failure blocks both check and codegen; record it but continue
        # so the report still shows the parse column as `fail`.
        return result

    # Check stage (Lachesis type/borrow checker), with cross-crate signatures.
    res = run([str(moira), "--check", *extern_args, str(lib)])
    result.check_ok = res.returncode == 0
    if res.returncode != 0:
        err = (res.stderr or res.stdout).strip().splitlines()
        snippet = err[-1] if err else "(no diagnostic)"
        # Count the diagnostics for a faster-at-a-glance signal.
        count = len([l for l in err if "type/borrow error" in l or "error" in l])
        note = f"check failed: {snippet}"
        if count:
            note = f"check failed ({count} diagnostic line(s)): {snippet}"
        result.notes.append(note)

    # Codegen probe (C backend) — independent of check; reports whether the
    # subset lowers + emits at all.
    safe = "".join(c if c.isalnum() or c in "-._" else "_" for c in crate["name"])
    res_c = run([str(moira), "--emit", "c", "-o", str(BUILD_DIR / f"{safe}.c"), str(lib)])
    result.codegen_ok = res_c.returncode == 0
    if res_c.returncode != 0:
        err = (res_c.stderr or res_c.stdout).strip().splitlines()
        snippet = err[-1] if err else "(no diagnostic)"
        result.notes.append(f"codegen failed: {snippet}")

    return result


def self_host(release: bool, strict: bool) -> int:
    BUILD_DIR.mkdir(exist_ok=True)
    moira = find_moira_binary(release) or build_host(release)
    if moira is None:
        log("cannot run self-host: host compiler is unavailable")
        return 1

    log("Stage 1: self-host readiness probe")
    results = [probe_crate(moira, c) for c in CRATES]

    ready = [r for r in results if r.self_hostable]
    blocked = [r for r in results if not r.self_hostable]

    # Write machine-readable report.
    payload = {
        "generated": datetime.now(timezone.utc).isoformat(),
        "moira_binary": str(moira),
        "crates": [
            {
                "crate": r.crate,
                "lib": r.lib,
                "parse_ok": r.parse_ok,
                "check_ok": r.check_ok,
                "codegen_ok": r.codegen_ok,
                "self_hostable": r.self_hostable,
                "notes": r.notes,
            }
            for r in results
        ],
        "summary": {"self_hostable": len(ready), "blocked": len(blocked), "total": len(results)},
    }
    REPORT_JSON.write_text(json.dumps(payload, indent=2))

    # Write human-readable report.
    lines = [
        "# Moira Stage-1 Self-Host Readiness",
        "",
        f"Generated: {payload['generated']}  ",
        f"Probe binary: `{moira.name}`",
        "",
        f"**Self-hostable: {len(ready)} / {len(results)}**  ",
        f"(Blocked: {len(blocked)})",
        "",
        "| Crate | Parse | Check | Codegen | Status |",
        "|-------|-------|-------|---------|--------|",
    ]
    for r in results:
        p = "ok" if r.parse_ok else "fail"
        c = "ok" if r.check_ok else "fail"
        g = "ok" if r.codegen_ok else "fail"
        status = "SELF-HOSTABLE" if r.self_hostable else "blocked"
        lines.append(f"| {r.crate} | {p} | {g} | {status} |")
    lines += ["", "## Notes", ""]
    for r in results:
        for n in r.notes:
            lines.append(f"- **{r.crate}**: {n}")
    if not any(r.notes for r in results):
        lines.append("- (no blockers recorded)")
    lines.append("")
    lines.append(
        "See `docs/SELF_HOSTING.md` for the blocking-subset gaps and the "
        "Stage-2 parity plan."
    )
    REPORT_MD.write_text("\n".join(lines))

    log(f"self-host report: {len(ready)}/{len(results)} crates self-hostable")
    log(f"  markdown -> {REPORT_MD}")
    log(f"  json     -> {REPORT_JSON}")

    return 1 if (strict and blocked) else 0


def workspace_version() -> str:
    """Best-effort version string: workspace `version` from Cargo.toml, else git
    describe, else `dev`."""
    cargo = ROOT / "Cargo.toml"
    if cargo.exists():
        text = cargo.read_text(encoding="utf-8")
        m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.MULTILINE)
        if m:
            return m.group(1)
    res = run(["git", "-C", str(ROOT), "describe", "--tags", "--always"], check=False)
    if res.returncode == 0 and res.stdout.strip():
        return res.stdout.strip()
    return "dev"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    h.update(path.read_bytes())
    return h.hexdigest()


def write_checksums(out: Path, version: str) -> Optional[Path]:
    """Write SHA-256 checksums for every staged artifact into
    `dist/tpt-moira-<version>-checksums.txt` (relative paths, BSD-style)."""
    artifacts = sorted(p for p in out.rglob("*") if p.is_file())
    if not artifacts:
        return None
    lines = []
    for p in artifacts:
        rel = p.relative_to(out).as_posix()
        lines.append(f"{sha256_file(p)}  {rel}")
    checksum_file = out / f"moira-{version}-checksums.txt"
    checksum_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
    log(f"checksums -> {checksum_file}")
    return checksum_file


def sign_artifacts(out: Path, key: Optional[str]) -> int:
    """Produce detached ASCII-armored GPG signatures for every staged artifact.

    Skips silently (with a warning) when `gpg` is unavailable. The signing key is
    selected with `--sign-key <id>`; otherwise gpg's default key is used. This
    satisfies the "signed/checksummed release artifacts" deliverable and aligns
    with `docs/STABILITY.md`.
    """
    gpg = shutil.which("gpg")
    if gpg is None:
        log("sign: `gpg` not found on PATH; skipping signatures (checksums only)")
        return 0
    args = [gpg, "--detach-sign", "--armor"]
    if key:
        args += ["--local-user", key]
    count = 0
    for p in sorted(x for x in out.rglob("*") if x.is_file()):
        sig = Path(str(p) + ".asc")
        res = run([*args, str(p)], check=False)
        if res.returncode == 0 and sig.exists():
            count += 1
        else:
            log(f"sign: failed to sign {p}")
    log(f"sign: wrote {count} detached signature(s)")
    return 0


def dist(release: bool, sign: bool, sign_key: Optional[str]) -> int:
    log("Building release binaries")
    res = run(["cargo", "build", "--workspace", "--release"], cwd=ROOT)
    if res.returncode != 0:
        sys.stderr.write(res.stdout + res.stderr)
        return 1
    out = ROOT / "dist"
    bin_dir = out / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    moira = find_moira_binary(True)
    if moira and moira.exists():
        shutil.copy(moira, bin_dir / moira.name)
    for lic in ("LICENSE-MIT", "LICENSE-APACHE-2.0"):
        src = ROOT / lic
        if src.exists():
            shutil.copy(src, out / lic)
    readme = ROOT / "README.md"
    if readme.exists():
        shutil.copy(readme, out / "README.md")
    version = workspace_version()
    # Stability policy: every release ships checksums (and, optionally, signed
    # artifacts) so users can verify integrity (see docs/STABILITY.md).
    write_checksums(out, version)
    if sign:
        sign_artifacts(out, sign_key)
    log(f"distributable staged at {out} (version {version})")
    return 0


def latest_tag() -> Optional[str]:
    res = run(["git", "-C", str(ROOT), "describe", "--tags", "--abbrev=0"], check=False)
    if res.returncode == 0:
        t = res.stdout.strip()
        return t or None
    return None


# Conventional-commit groups used by the changelog generator.
CHANGELOG_GROUPS = [
    ("feat", "Features"),
    ("fix", "Bug Fixes"),
    ("perf", "Performance"),
    ("docs", "Documentation"),
    ("refactor", "Refactoring"),
    ("test", "Tests"),
    ("chore", "Chores"),
    ("other", "Other"),
]


def changelog(release: bool) -> int:
    """Generate a changelog from git history since the last tag (or from the
    beginning if untagged), grouped by conventional-commit type. This backs the
    "automated changelog generation" deliverable in docs/STABILITY.md.
    """
    since = latest_tag()
    if since:
        log_cmd = ["git", "-C", str(ROOT), "log", f"{since}..HEAD", "--pretty=format:%s"]
        scope = f"since {since}"
    else:
        log_cmd = ["git", "-C", str(ROOT), "log", "--pretty=format:%s"]
        scope = "full history"
    res = run(log_cmd, check=False)
    if res.returncode != 0:
        log("changelog: could not read git history")
        return 1
    subjects = [l.strip() for l in res.stdout.splitlines() if l.strip()]
    grouped: dict[str, list[str]] = {k: [] for k, _ in CHANGELOG_GROUPS}
    for subj in subjects:
        m = re.match(r"(\w+)(?:\([^)]*\))?!?:\s*(.*)", subj)
        kind = m.group(1).lower() if m else "other"
        body = m.group(2) if m else subj
        grouped.setdefault(kind, grouped["other"]).append(body)
    version = workspace_version()
    lines = [f"# Moira {version} changelog", "", f"_Changes {scope}._", ""]
    for key, title in CHANGELOG_GROUPS:
        items = grouped.get(key) or []
        if not items:
            continue
        lines += [f"## {title}", ""]
        for it in items:
            lines.append(f"- {it}")
        lines.append("")
    text = "\n".join(lines).rstrip() + "\n"
    out_path = ROOT / "dist" / f"moira-{version}-CHANGELOG.md"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(text, encoding="utf-8")
    log(f"changelog -> {out_path} ({len(subjects)} commit(s) {scope})")
    print(text)
    return 0


def docs(release: bool, out: Optional[str]) -> int:
    """Build a versioned static documentation site under ``dist/docs`` (or
    ``--out``). This backs the "publish versioned docs tied to releases"
    deliverable: every release bundles the architecture guide, self-hosting
    status, stability policy, contributor guide, TODO/roadmap, README, the
    generated changelog, and the latest self-host probe report — as plain HTML
    (no external toolchain required) plus the raw markdown sources.
    """
    version = workspace_version()
    outdir = Path(out) if out else ROOT / "dist" / "docs"
    md_dir = outdir / "md"
    html_dir = outdir / "html"
    md_dir.mkdir(parents=True, exist_ok=True)
    html_dir.mkdir(parents=True, exist_ok=True)

    # (source path, site title). Missing files are skipped with a warning.
    pages = [
        ("README.md", "Overview"),
        ("docs/ARCHITECTURE.md", "Architecture"),
        ("docs/SELF_HOSTING.md", "Self-Hosting Status"),
        ("docs/STABILITY.md", "Stability Policy"),
        ("CONTRIBUTING.md", "Contributing"),
        ("TODO.md", "Roadmap / TODO"),
    ]
    # Include the freshly generated changelog when it exists.
    changelog_path = ROOT / "dist" / f"moira-{version}-CHANGELOG.md"
    entries: list[tuple[str, str, str]] = []  # (html filename, title, source rel path)
    for rel, title in pages:
        src = ROOT / rel
        if not src.exists():
            log(f"docs: warning — {rel} not found, skipping")
            continue
        text = src.read_text(encoding="utf-8")
        (md_dir / src.name).write_text(text, encoding="utf-8")
        html_name = src.stem.lower().replace("_", "-") + ".html"
        (html_dir / html_name).write_text(
            render_html_page(title, md_to_html(text), version), encoding="utf-8"
        )
        entries.append((html_name, title, rel))
    if changelog_path.exists():
        text = changelog_path.read_text(encoding="utf-8")
        (md_dir / changelog_path.name).write_text(text, encoding="utf-8")
        (html_dir / "changelog.html").write_text(
            render_html_page("Changelog", md_to_html(text), version), encoding="utf-8"
        )
        entries.append(("changelog.html", "Changelog", changelog_path.name))
    # Snapshot the latest self-host probe report when present.
    report = ROOT / "build" / "self_host_report.md"
    if report.exists():
        text = report.read_text(encoding="utf-8")
        (md_dir / "self-host-report.md").write_text(text, encoding="utf-8")
        (html_dir / "self-host-report.html").write_text(
            render_html_page("Self-Host Probe Report", md_to_html(text), version),
            encoding="utf-8",
        )
        entries.append(("self-host-report.html", "Self-Host Probe Report", "build/self_host_report.md"))
    # Version stamp so deployed sites can keep one directory per release.
    (outdir / "version.txt").write_text(version + "\n", encoding="utf-8")

    nav = "".join(f'<a href="html/{name}">{title}</a>' for name, title, _ in entries)
    body = ["<h1>Moira documentation</h1>",
            f'<p class="meta">Version {version} &middot; TPT Solutions</p>',
            "<ul>"]
    for name, title, rel in entries:
        body.append(
            f'<li><a href="html/{name}">{title}</a>'
            f' <span class="meta">({rel})</span></li>'
        )
    body.append("</ul>")
    (outdir / "index.html").write_text(
        render_html_page("Moira documentation", "\n".join(body), version, nav=nav),
        encoding="utf-8",
    )
    log(f"docs site ({len(entries) + 1} pages, version {version}) -> {outdir}")
    return 0


def render_html_page(title: str, body_html: str, version: str, nav: str = "") -> str:
    """Wrap converted markdown in a minimal, dependency-free HTML shell."""
    nav_html = f"<nav>{nav}</nav>" if nav else ""
    return (
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n"
        f"<title>{title} — Moira {version}</title>\n"
        "<style>\n"
        "body { font-family: system-ui, sans-serif; max-width: 60rem;"
        " margin: 2rem auto; padding: 0 1rem; line-height: 1.55; color: #1a1a1a; }\n"
        "nav a { margin-right: 1rem; }\n"
        "pre { background: #f4f4f4; padding: 0.75rem; overflow-x: auto;"
        " border-radius: 4px; }\n"
        "code { background: #f4f4f4; padding: 0.1rem 0.3rem; border-radius: 3px; }\n"
        "pre code { background: none; padding: 0; }\n"
        ".meta { color: #666; font-size: 0.9em; }\n"
        "</style>\n</head>\n<body>\n"
        f"{nav_html}\n{body_html}\n</body>\n</html>\n"
    )


def escape_html(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def md_inline(text: str) -> str:
    """Inline Markdown (code spans, bold, italic, links) to HTML."""
    text = escape_html(text)
    text = re.sub(r"`([^`]+)`", r"<code>\1</code>", text)
    text = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", text)
    text = re.sub(r"(?<!\*)\*([^*]+)\*(?!\*)", r"<em>\1</em>", text)
    text = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", r'<a href="\2">\1</a>', text)
    return text


def md_to_html(md: str) -> str:
    """Convert a subset of Markdown to HTML (no external dependencies).

    Handles: fenced code blocks, ATX headings, bullet/numbered lists, block
    quotes, horizontal rules, paragraphs, and inline formatting (bold, italic,
    inline code, links). Good enough for the project's documentation set.
    """
    out: list[str] = []
    in_code = False
    code_buf: list[str] = []
    list_stack: list[str] = []  # "ul" or "ol"
    para_buf: list[str] = []
    last_li_idx: Optional[int] = None

    def flush_para() -> None:
        nonlocal last_li_idx
        if para_buf:
            inline = md_inline(" ".join(para_buf))
            if list_stack and last_li_idx is not None:
                # A continuation line of the previous list item (Markdown soft
                # wrap): fold it into that item instead of breaking the list.
                out[last_li_idx] = out[last_li_idx][:-len("</li>")] + " " + inline + "</li>"
            else:
                out.append(f"<p>{inline}</p>")
            para_buf.clear()

    def close_lists() -> None:
        while list_stack:
            out.append(f"</{list_stack.pop()}>")

    for raw in md.splitlines():
        line = raw.rstrip()
        if line.strip().startswith("```"):
            flush_para()
            close_lists()
            if in_code:
                if code_buf:
                    out.append(escape_html("\n".join(code_buf)))
                out.append("</code></pre>")
                in_code = False
            else:
                in_code = True
                code_buf = []
                out.append("<pre><code>")
            continue
        if in_code:
            code_buf.append(raw)
            continue
        stripped = line.strip()
        if not stripped:
            flush_para()
            close_lists()
            continue
        m = re.match(r"^(#{1,6})\s+(.*)$", stripped)
        if m:
            flush_para()
            close_lists()
            level = len(m.group(1))
            out.append(f"<h{level}>{md_inline(m.group(2))}</h{level}>")
            continue
        if re.match(r"^(-{3,}|\*{3,})$", stripped):
            flush_para()
            close_lists()
            out.append("<hr>")
            continue
        m = re.match(r"^[-*+]\s+(.*)$", stripped)
        if m:
            flush_para()
            if not list_stack or list_stack[-1] != "ul":
                close_lists()
                list_stack.append("ul")
                out.append("<ul>")
            out.append(f"<li>{md_inline(m.group(1))}</li>")
            last_li_idx = len(out) - 1
            continue
        m = re.match(r"^\d+[.)]\s+(.*)$", stripped)
        if m:
            flush_para()
            if not list_stack or list_stack[-1] != "ol":
                close_lists()
                list_stack.append("ol")
                out.append("<ol>")
            out.append(f"<li>{md_inline(m.group(1))}</li>")
            last_li_idx = len(out) - 1
            continue
        if stripped.startswith(">"):
            flush_para()
            close_lists()
            out.append(
                "<blockquote><p>"
                + md_inline(stripped.lstrip("> ").rstrip())
                + "</p></blockquote>"
            )
            continue
        para_buf.append(stripped)
    if in_code:
        out.append(escape_html("\n".join(code_buf)))
        out.append("</code></pre>")
    flush_para()
    close_lists()
    return "\n".join(out)


# Emit modes whose output must be byte-identical across stages, with the sample
# source each one is exercised on. `asm` must avoid composite types because the
# from-scratch native backend only handles the integer/control-flow subset.
DETERMINISM_MODES = ["mir", "ssa", "c", "asm"]

DETERMINISM_STRUCT_SAMPLE = """
struct Point { x: i32, y: i32 }

fn add(a: i32, b: i32) -> i32 { a + b }

fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }

fn make() -> Point {
    let p = Point { x: 1, y: 2 };
    let z = p.x + p.y;
    Point { x: z, y: z }
}

fn sum_to(n: i32) -> i32 {
    let mut s = 0;
    let mut i = 0;
    while i < n { s = s + i; i = i + 1; }
    s
}

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
    print_i32(sum_to(5));
    let p = make();
    print_i32(p.x);
}
"""

DETERMINISM_ASM_SAMPLE = """
fn add(a: i32, b: i32) -> i32 { a + b }

fn max(a: i32, b: i32) -> i32 { if a > b { a } else { b } }

fn sum_to(n: i32) -> i32 {
    let mut s = 0;
    let mut i = 0;
    while i < n { s = s + i; i = i + 1; }
    s
}

fn main() {
    print_i32(add(3, 4));
    print_i32(max(7, 10));
    print_i32(sum_to(5));
}
"""


def determinism(release: bool) -> int:
    """Stage-2 cross-process parity: the same source compiled by two separate
    `moira` processes (each with its own randomized hash-seed) must yield
    byte-identical MIR/SSA/C/ASM. This is the exact condition that guarantees
    stage-1 == stage-2 output parity, so it is the gate for self-hosting.

    The incremental cache is bypassed via `MOIRA_NO_CACHE=1` so both runs
    recompute the emit from scratch (a cached first run would otherwise mask
    any nondeterminism).
    """
    BUILD_DIR.mkdir(exist_ok=True)
    moira = find_moira_binary(release) or build_host(release)
    if moira is None:
        log("cannot run determinism check: host compiler is unavailable")
        return 1

    samples = {
        "mir": DETERMINISM_STRUCT_SAMPLE,
        "ssa": DETERMINISM_STRUCT_SAMPLE,
        "c": DETERMINISM_STRUCT_SAMPLE,
        "asm": DETERMINISM_ASM_SAMPLE,
    }

    no_cache_env = {**os.environ, "MOIRA_NO_CACHE": "1"}
    ok = True
    for mode in DETERMINISM_MODES:
        path = BUILD_DIR / f"determinism_{mode}.rs"
        path.write_text(samples[mode])

        def emit_once() -> tuple[Optional[str], Optional[str]]:
            res = run(
                [str(moira), "--emit", mode, str(path)],
                env=no_cache_env,
            )
            if res.returncode != 0:
                return None, (res.stderr or res.stdout).strip()
            return res.stdout, None

        first, err1 = emit_once()
        second, err2 = emit_once()
        if first is None or second is None:
            log(f"determinism[{mode}]: EMIT FAILED -> {err1 or err2}")
            ok = False
            continue
        if first == second:
            log(f"determinism[{mode}]: OK (byte-identical across two processes)")
        else:
            log(f"determinism[{mode}]: FAIL — output differs across processes")
            ok = False

    return 0 if ok else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Moira bootstrap orchestrator")
    parser.add_argument(
        "command",
        nargs="?",
        default="all",
        choices=["build", "check", "test", "self-host", "dist", "determinism", "changelog", "docs", "all"],
    )
    parser.add_argument("--release", action="store_true", help="use the release profile")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="self-host: fail if any crate is not fully self-hostable",
    )
    parser.add_argument(
        "--sign",
        action="store_true",
        help="dist: also produce detached GPG signatures for staged artifacts",
    )
    parser.add_argument(
        "--sign-key",
        default=None,
        help="dist: GPG key id to sign with (implies --sign)",
    )
    parser.add_argument("--out", default=None, help="docs: output directory (default dist/docs)")
    args = parser.parse_args(argv)

    if args.command in ("build", "all"):
        res = run(["cargo", "build", "--workspace", *(["--release"] if args.release else [])], cwd=ROOT)
        if res.returncode != 0:
            sys.stderr.write(res.stdout + res.stderr)
            return 1
    if args.command in ("check", "all"):
        res = run(["cargo", "check", "--workspace"], cwd=ROOT)
        if res.returncode != 0:
            sys.stderr.write(res.stdout + res.stderr)
            return 1
    if args.command in ("test", "all"):
        res = run(["cargo", "test", "--workspace"], cwd=ROOT)
        if res.returncode != 0:
            sys.stderr.write(res.stdout + res.stderr)
            return 1
    if args.command == "self-host":
        return self_host(args.release, args.strict or os.environ.get("MOIRA_SELFHOST_STRICT") == "1")
    if args.command == "determinism":
        return determinism(args.release)
    if args.command == "changelog":
        return changelog(args.release)
    if args.command == "docs":
        return docs(args.release, args.out)
    if args.command == "dist":
        sign = args.sign or args.sign_key is not None
        return dist(args.release, sign, args.sign_key)

    log("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
