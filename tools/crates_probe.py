#!/usr/bin/env python3
"""Bulk-probe real crates.io crates with Moira and track the pass rate.

For each crate in the sample this runs, in order:

  1. `moira --emit ast <lib.rs>`  -- parse only (pass/fail = "parses").
  2. `moira --check <lib.rs>`     -- type + borrow check (pass/fail = "checks").

Each run is wall-clock limited and memory-capped (a runaway checker must not
take the machine down: see the tuple-index parser hang fixed in clotho). The
probe records per-crate outcome, timing, peak memory and the diagnostics, then:

  * triages the gaps: each diagnostic is normalised to a *class* (identifiers
    and literals stripped) and each class is credited with the crates it alone
    blocks, so the highest-leverage gap is listed first;
  * appends a summary line to `history.jsonl`, so the pass-rate trend can be
    tracked across runs (TODO: "Track pass-rate trend over time").

Outputs go to `build/crates_probe/`:
  results.json   per-crate detail from this run
  report.md      human-readable report (pass rates + triage)
  history.jsonl  one summary line per run (appended)

Usage:
  python tools/crates_probe.py                       # default sample
  python tools/crates_probe.py --moira target/debug/moira.exe
  python tools/crates_probe.py --crates serde-1.0.229 itoa-1.0.19
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "build" / "crates_probe"

# Representative sample spanning sizes and language features: tiny leaf crates,
# mid-size utilities, macro-heavy derive crates, and large async/regex crates.
DEFAULT_SAMPLE = [
    "itoa-1.0.19",
    "cfg-if-1.0.5",
    "scopeguard-1.2.0",
    "lazy_static-1.5.1",
    "once_cell-1.21.4",
    "either-1.19.0",
    "memchr-2.8.3",
    "bitflags-2.13.2",
    "smallvec-1.16.5",
    "anyhow-1.0.104",
    "log-0.4.35",
    "bytes-1.12.1",
    "thiserror-2.0.21",
    "serde-1.0.229",
    "hashbrown-0.17.1",
    "regex-syntax-0.8.11",
    "regex-1.13.1",
    "clap-4.6.7",
    "tokio-1.54.1",
]

# A wall-clock and memory budget per moira invocation.
TIMEOUT_SECS = 120
MEMORY_CAP_MB = 2048


def default_registry_src() -> Path | None:
    """Locate the cargo registry source directory (where extracted crates live)."""
    home = Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")
    base = home / "registry" / "src"
    if not base.is_dir():
        return None
    # Prefer the crates.io index directory; there may be several mirrors.
    candidates = sorted(base.glob("index.crates.io-*"))
    return candidates[0] if candidates else None


def run_capped(args: list[str], env: dict[str, str]) -> dict:
    """Run a command with a wall-clock timeout and a resident-memory cap.

    Returns exit code, stderr/stdout text, peak memory (MB), elapsed seconds and
    whether the run was killed (by the memory cap or the timeout).
    """
    import psutil  # imported here so the rest of the script works without it

    with tempfile.TemporaryFile() as out_f, tempfile.TemporaryFile() as err_f:
        start = time.monotonic()
        proc = subprocess.Popen(args, stdout=out_f, stderr=err_f, env=env)
        peak_mb = 0.0
        killed = None
        try:
            ps = psutil.Process(proc.pid)
        except psutil.Error:
            ps = None
        while proc.poll() is None:
            time.sleep(0.05)
            if ps is not None:
                try:
                    mem = ps.memory_info().rss / (1024 * 1024)
                    peak_mb = max(peak_mb, mem)
                    if mem > MEMORY_CAP_MB:
                        killed = f"memory > {MEMORY_CAP_MB} MB"
                except psutil.Error:
                    pass
            if killed is None and time.monotonic() - start > TIMEOUT_SECS:
                killed = f"timeout > {TIMEOUT_SECS} s"
            if killed is not None:
                proc.kill()
                proc.wait()
                break
        elapsed = time.monotonic() - start
        out_f.seek(0)
        err_f.seek(0)
        stdout = out_f.read().decode("utf-8", "replace")
        stderr = err_f.read().decode("utf-8", "replace")
    return {
        "exit": proc.returncode,
        "stdout": stdout,
        "stderr": stderr,
        "peak_mb": round(peak_mb, 1),
        "secs": round(elapsed, 2),
        "killed": killed,
    }


DIAG_RE = re.compile(r"^moira: (?:Error(?: at bytes [0-9-]+)?: )?(?P<msg>.+)$")


def diagnostics(stderr: str) -> list[str]:
    """Extract the individual diagnostic messages from moira's stderr."""
    msgs = []
    for line in stderr.splitlines():
        m = DIAG_RE.match(line.strip())
        if not m:
            continue
        msg = m.group("msg")
        # Skip the summary line; it is not a diagnostic itself.
        if msg.startswith("aborting:"):
            continue
        # A `  at <file>:<line>:<col>` line locates the diagnostic above it; it is not a
        # diagnostic of its own (counting it would make every location a gap class).
        if LOCATION_RE.match(msg):
            continue
        msgs.append(msg)
    return msgs


LOCATION_RE = re.compile(r"^\s*at .+:\d+:\d+$")


def classify(msg: str) -> str:
    """Normalise a diagnostic to a gap class.

    Positions, identifiers and the 'found ...' token are dropped, so every
    'expected item, found identifier X' lands in the same class as
    'expected item, found identifier Y': the class names the construct the front
    end does not accept, not the particular token.
    """
    s = re.sub(r" \(in `[^`]*`\)", "", msg)
    s = re.sub(r"^(parse error: )?(Error at bytes [0-9-]+: |Error: )?", "", s)
    s = re.sub(r", found .*$", "", s)
    s = re.sub(r"`[^`]*`", "`_`", s)
    s = re.sub(r"at bytes [0-9-]+", "", s)
    s = re.sub(r"\d+", "N", s)
    return s.strip()


BYTES_RE = re.compile(r"at bytes (\d+)")


FILE_RE = re.compile(r"\(in `([^`]*)`\)")


def parse_progress(lib: Path, msgs: list[str]) -> float | None:
    """Approximate share (0..1) of `lib.rs` parsed before the first error.

    The offset comes from the diagnostic, so it can belong to a `mod` file
    instead; this is a trend indicator, not an exact measure.
    """
    for m in msgs:
        hit = BYTES_RE.search(m)
        if hit:
            where = FILE_RE.search(m)
            path = Path(where.group(1)) if where else lib
            size = max(path.stat().st_size, 1)
            return round(min(int(hit.group(1)) / size, 1.0), 3)
    return None


def probe_crate(moira: Path, lib: Path, env: dict[str, str]) -> dict:
    rec: dict = {"lib": str(lib)}
    parse = run_capped([str(moira), "--emit", "ast", str(lib)], env)
    rec["parse_ok"] = parse["exit"] == 0 and not parse["killed"]
    if rec["parse_ok"]:
        rec["parse_progress"] = 1.0
    rec["parse"] = {k: parse[k] for k in ("peak_mb", "secs", "killed")}
    if not rec["parse_ok"]:
        msgs = diagnostics(parse["stderr"])
        rec["diagnostics"] = msgs or [f"parse failed: {(parse['stderr'] or parse['killed'] or '?').strip().splitlines()[-1:]}"]
        rec["parse_progress"] = parse_progress(lib, rec["diagnostics"])
        rec["check_ok"] = False
        rec["check"] = None
        return rec

    chk = run_capped([str(moira), "--check", str(lib)], env)
    rec["check_ok"] = chk["exit"] == 0 and not chk["killed"]
    rec["check"] = {k: chk[k] for k in ("peak_mb", "secs", "killed")}
    if chk["killed"]:
        rec["diagnostics"] = [f"check killed: {chk['killed']}"]
    elif not rec["check_ok"]:
        rec["diagnostics"] = diagnostics(chk["stderr"]) or [
            "check failed with no diagnostic"
        ]
    else:
        rec["diagnostics"] = []
    return rec


def triage(results: dict[str, dict]) -> list[dict]:
    """Rank gap classes by how many crates they alone block."""
    crate_classes: dict[str, set[str]] = {}
    class_crates: dict[str, set[str]] = defaultdict(set)
    counts: Counter[str] = Counter()
    for name, rec in results.items():
        classes = {classify(m) for m in rec.get("diagnostics", [])}
        crate_classes[name] = classes
        for c in classes:
            class_crates[c].add(name)
        for m in rec.get("diagnostics", []):
            counts[classify(m)] += 1
    rows = []
    for cls, crates in class_crates.items():
        sole = [n for n in crates if crate_classes[n] == {cls}]
        rows.append(
            {
                "class": cls,
                "crates_affected": len(crates),
                "sole_blocker_for": len(sole),
                "occurrences": counts[cls],
            }
        )
    rows.sort(key=lambda r: (-r["sole_blocker_for"], -r["crates_affected"], -r["occurrences"]))
    return rows


def _pct(value: float | None) -> str:
    return "-" if value is None else f"{value:.0%}"


def mean_progress(results: dict[str, dict]) -> float:
    vals = [r["parse_progress"] for r in results.values() if r.get("parse_progress") is not None]
    return sum(vals) / len(vals) if vals else 0.0


def write_report(results: dict[str, dict], rows: list[dict], summary: dict) -> str:
    total = len(results)
    parsed = sum(1 for r in results.values() if r.get("parse_ok"))
    checked = sum(1 for r in results.values() if r.get("check_ok"))
    lines = [
        "# Moira crates.io probe",
        "",
        f"Generated: {summary['timestamp']}  ",
        f"Commit: `{summary.get('commit') or 'unknown'}`",
        "",
        f"**Parse pass rate: {parsed}/{total}**  ",
        f"**Check pass rate: {checked}/{total}**",
        "",
        "## Per crate",
        "",
        f"**Mean parse progress: {mean_progress(results):.1%}** "
        "(share of each `lib.rs` parsed before its first error; approximate)  ",
        "",
        "| Crate | Parse | Parsed to | Check | Diagnostics | Time (s) | Peak MB |",
        "|-------|-------|-----------|-------|-------------|----------|---------|",
    ]
    for name, r in results.items():
        peak = max(
            (r.get("parse") or {}).get("peak_mb", 0),
            (r.get("check") or {}).get("peak_mb", 0),
        )
        secs = round(
            (r.get("parse") or {}).get("secs", 0) + (r.get("check") or {}).get("secs", 0), 2
        )
        lines.append(
            f"| {name} | {'ok' if r.get('parse_ok') else 'fail'} | "
            f"{_pct(r.get('parse_progress'))} | "
            f"{'ok' if r.get('check_ok') else 'fail'} | {len(r.get('diagnostics', []))} | "
            f"{secs} | {peak} |"
        )
    lines += [
        "",
        "## Gap triage (highest leverage first)",
        "",
        "`sole blocker` = crates whose *only* gap class is this one, i.e. the crates",
        "that would pass if this class were closed. Ranking is by that number.",
        "The parser stops at the first error in each file, so each crate contributes",
        "one blocking class here; later gaps only appear once earlier ones are fixed.",
        "",
        "| Class | Crates affected | Sole blocker for | Occurrences |",
        "|-------|-----------------|------------------|-------------|",
    ]
    examples: dict[str, str] = {}
    for rec in results.values():
        for m in rec.get("diagnostics", []):
            examples.setdefault(classify(m), m)
    lines[-1:-1] = []
    lines[-1] = lines[-1]
    for row in rows[:25]:
        lines.append(
            f"| {row['class']} | {row['crates_affected']} | {row['sole_blocker_for']} | {row['occurrences']} |"
        )
    lines += ["", "### Example diagnostic per class", ""]
    for row in rows[:25]:
        lines.append(f"- `{row['class']}`: {examples.get(row['class'], '')}")
    return "\n".join(lines) + "\n"


def git_commit() -> str | None:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    default_moira = ROOT / "target" / "debug" / ("moira.exe" if os.name == "nt" else "moira")
    ap.add_argument("--moira", type=Path, default=default_moira, help="moira binary to probe")
    ap.add_argument("--registry", type=Path, default=None, help="cargo registry src dir")
    ap.add_argument("--crates", nargs="*", default=None, help="crate dirs (name-version) to probe")
    args = ap.parse_args()

    try:
        import psutil  # noqa: F401
    except ImportError:
        print("crates_probe: needs `psutil` (pip install psutil) for the memory cap", file=sys.stderr)
        return 2
    if not args.moira.exists():
        print(f"crates_probe: moira binary not found: {args.moira}", file=sys.stderr)
        return 2
    registry = args.registry or default_registry_src()
    if registry is None or not registry.is_dir():
        print("crates_probe: no cargo registry found; run `cargo fetch` for the sample first", file=sys.stderr)
        return 2

    names = args.crates or DEFAULT_SAMPLE
    env = dict(os.environ, MOIRA_NO_CACHE="1")
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    results: dict[str, dict] = {}
    for name in names:
        crate_dir = registry / name
        lib = crate_dir / "src" / "lib.rs"
        if not lib.exists():
            results[name] = {"parse_ok": False, "check_ok": False, "diagnostics": ["missing src/lib.rs in registry"], "parse": None, "check": None}
            print(f"  {name}: missing", flush=True)
            continue
        rec = probe_crate(args.moira, lib, env)
        results[name] = rec
        status = "check ok" if rec["check_ok"] else ("parse ok" if rec["parse_ok"] else "parse fail")
        print(f"  {name}: {status} ({len(rec['diagnostics'])} diagnostic(s))", flush=True)

    rows = triage(results)
    timestamp = dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")
    summary = {
        "timestamp": timestamp,
        "commit": git_commit(),
        "crates": len(results),
        "parse_ok": sum(1 for r in results.values() if r.get("parse_ok")),
        "check_ok": sum(1 for r in results.values() if r.get("check_ok")),
        "mean_parse_progress": round(mean_progress(results), 4),
    }
    (OUT_DIR / "results.json").write_text(
        json.dumps({"summary": summary, "crates": results, "triage": rows}, indent=2),
        encoding="utf-8",
    )
    (OUT_DIR / "report.md").write_text(write_report(results, rows, summary), encoding="utf-8")
    with (OUT_DIR / "history.jsonl").open("a", encoding="utf-8") as f:
        f.write(json.dumps(summary) + "\n")

    print(f"parse {summary['parse_ok']}/{summary['crates']}, check {summary['check_ok']}/{summary['crates']}")
    print(f"report: {OUT_DIR / 'report.md'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
