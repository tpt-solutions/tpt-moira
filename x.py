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
  dist        build release binaries and stage a distributable tree.
  all         build + check + test (default).

The self-host probe never fails the build: it always produces a report under
`build/self_host_report.md` and exits 0 so it can run in CI as a tracking job.
When real self-hosting is achievable, set MOIRA_SELFHOST_STRICT=1 to make the
probe fail on any crate that does not fully self-host.
"""

from __future__ import annotations

import argparse
import json
import os
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
        "path": "moira-alloc",
        "lib": "moira-alloc/src/lib.rs",
        "deps": [],
    },
    {
        "name": "clotho",
        "path": "clotho",
        "lib": "clotho/src/lib.rs",
        "deps": ["moira-alloc"],
    },
    {
        "name": "atropos",
        "path": "atropos",
        "lib": "atropos/src/lib.rs",
        "deps": ["clotho", "moira-alloc"],
    },
    {
        "name": "lachesis",
        "path": "lachesis",
        "lib": "lachesis/src/lib.rs",
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


def run(cmd: list[str], cwd: Optional[Path] = None, check: bool = False) -> subprocess.CompletedProcess:
    log(" ".join(str(c) for c in cmd))
    return subprocess.run(cmd, cwd=str(cwd) if cwd else None, capture_output=True, text=True)


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

    # Parse + check (Lachesis type/borrow checker).
    res = run([str(moira), "--check", str(lib)])
    result.parse_ok = res.returncode == 0
    result.check_ok = res.returncode == 0
    if res.returncode != 0:
        err = (res.stderr or res.stdout).strip().splitlines()
        snippet = err[-1] if err else "(no diagnostic)"
        result.notes.append(f"check failed: {snippet}")

    # Codegen probe (C backend) — independent of check; reports whether the
    # subset lowers + emits at all.
    safe = "".join(c if c.isalnum() or c in "-._" else "_" for c in crate["name"])
    res_c = run([str(moira), "--emit", "c", "-o", str(BUILD_DIR / f"{safe}.c"), str(lib)])
    result.codegen_ok = res_c.returncode == 0
    if res_c.returncode != 0 and result.parse_ok:
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
        "| Crate | Parse/Check | Codegen | Status |",
        "|-------|-------------|---------|--------|",
    ]
    for r in results:
        p = "ok" if r.check_ok else "fail"
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


def dist(release: bool) -> int:
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
    log(f"distributable staged at {out}")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Moira bootstrap orchestrator")
    parser.add_argument(
        "command",
        nargs="?",
        default="all",
        choices=["build", "check", "test", "self-host", "dist", "all"],
    )
    parser.add_argument("--release", action="store_true", help="use the release profile")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="self-host: fail if any crate is not fully self-hostable",
    )
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
    if args.command == "dist":
        return dist(args.release)

    log("done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
