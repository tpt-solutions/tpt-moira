#!/usr/bin/env python3
"""Probe a Linux kernel source tree's Rust code with Moira (Rust-for-Linux scope).

The Rust-for-Linux code is one crate, `rust/kernel`, whose root `lib.rs` declares
the rest of the kernel as `mod` files, plus out-of-tree-style samples in
`samples/rust/*.rs` that import it as `kernel::...`. This probe measures both:

  1. the kernel crate itself  -- `moira --emit ast`, then `moira --check`;
  2. each sample              -- `moira --emit ast`, then `moira --check` with the
                                 kernel crate passed as `--extern kernel=...`.

Per-file runs are wall-clock limited and memory-capped (see `crates_probe.py`).
Diagnostics are normalised and triaged with the same classifier as the crates
probe, so the highest-leverage gap is listed first.

Outputs go to `build/kernel_probe/` (`report.md`, `results.json`, `history.jsonl`).
When no kernel tree is given the probe writes a "blocked" report and exits 2:
the kernel source is not vendored in this repository.

Usage:
  python tools/kernel_probe.py --kernel-tree /path/to/linux
  set MOIRA_KERNEL_TREE=/path/to/linux && python tools/kernel_probe.py
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

# Reuse the crates probe's capped runner, diagnostic parsing and triage.
from crates_probe import diagnostics, run_capped, triage  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "build" / "kernel_probe"
KERNEL_CRATE_ROOT = Path("rust") / "kernel" / "lib.rs"
SAMPLES_DIR = Path("samples") / "rust"


def discover(tree: Path) -> tuple[Path | None, list[Path]]:
    """The kernel crate root and the sample files that import it (both may be absent)."""
    kernel_lib = tree / KERNEL_CRATE_ROOT
    samples = sorted((tree / SAMPLES_DIR).glob("*.rs")) if (tree / SAMPLES_DIR).is_dir() else []
    return (kernel_lib if kernel_lib.is_file() else None), samples


def probe_unit(moira: Path, src: Path, externs: list[str], env: dict[str, str]) -> dict:
    """Parse, then check, one unit. `externs` are `name=path` pairs passed through."""
    rec: dict = {"file": str(src)}
    parse = run_capped([str(moira), "--emit", "ast", str(src)], env)
    rec["parse_ok"] = parse["exit"] == 0 and not parse["killed"]
    rec["parse"] = {k: parse[k] for k in ("peak_mb", "secs", "killed")}
    if not rec["parse_ok"]:
        rec["check_ok"] = False
        rec["diagnostics"] = diagnostics(parse["stderr"]) or [
            "parse failed: " + ((parse["stderr"] or parse["killed"] or "?").strip().splitlines() or ["?"])[-1]
        ]
        return rec
    extern_args: list[str] = []
    for e in externs:
        extern_args += ["--extern", e]
    chk = run_capped([str(moira), "--check", *extern_args, str(src)], env)
    rec["check_ok"] = chk["exit"] == 0 and not chk["killed"]
    rec["check"] = {k: chk[k] for k in ("peak_mb", "secs", "killed")}
    if chk["killed"]:
        rec["diagnostics"] = [f"check killed: {chk['killed']}"]
    elif not rec["check_ok"]:
        rec["diagnostics"] = diagnostics(chk["stderr"]) or ["check failed with no diagnostic"]
    else:
        rec["diagnostics"] = []
    return rec


def write_blocked(reason: str) -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    text = (
        "# Linux kernel (Rust-for-Linux) validation: not run\n\n"
        f"Status: **blocked** — {reason}\n\n"
        "Scope: the kernel crate `rust/kernel/lib.rs` and the samples in `samples/rust/`.\n"
        "Run `python tools/kernel_probe.py --kernel-tree <path-to-linux-source>` once a\n"
        "kernel source tree is available.\n"
    )
    (OUT_DIR / "report.md").write_text(text, encoding="utf-8")


def write_report(units: dict[str, dict], rows: list[dict], summary: dict, tree: Path) -> str:
    total = len(units)
    lines = [
        "# Linux kernel (Rust-for-Linux) validation",
        "",
        f"Kernel tree: `{tree}`  ",
        f"Generated: {summary['timestamp']}  ",
        f"Units: {total} — parse {summary['parse_ok']}/{total}, check {summary['check_ok']}/{total}",
        "",
        "| Unit | Parse | Check | Diagnostics |",
        "|------|-------|-------|-------------|",
    ]
    for name, rec in units.items():
        parse = "ok" if rec.get("parse_ok") else "fail"
        check = "ok" if rec.get("check_ok") else "fail"
        lines.append(f"| `{name}` | {parse} | {check} | {len(rec.get('diagnostics', []))} |")
    lines += ["", "## Gap triage (highest leverage first)", ""]
    if not rows:
        lines.append("No diagnostics: every unit parses and checks.")
    else:
        lines += ["| Gap class | Units affected | Sole blocker for | Occurrences |", "|---|---|---|---|"]
        for r in rows[:25]:
            lines.append(
                f"| {r['class']} | {r['crates_affected']} | {r['sole_blocker_for']} | {r['occurrences']} |"
            )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    default_moira = ROOT / "target" / "release" / ("moira.exe" if os.name == "nt" else "moira")
    if not default_moira.exists():
        default_moira = ROOT / "target" / "debug" / ("moira.exe" if os.name == "nt" else "moira")
    ap.add_argument("--kernel-tree", type=Path, default=None, help="root of a Linux kernel source tree")
    ap.add_argument("--moira", type=Path, default=default_moira, help="moira binary to probe")
    args = ap.parse_args()

    tree = args.kernel_tree or (Path(os.environ["MOIRA_KERNEL_TREE"]) if os.environ.get("MOIRA_KERNEL_TREE") else None)
    if tree is None:
        write_blocked("no kernel source tree was given (`--kernel-tree` or `MOIRA_KERNEL_TREE`)")
        print("kernel_probe: no kernel tree given; wrote build/kernel_probe/report.md (blocked)", file=sys.stderr)
        return 2
    if not (tree / KERNEL_CRATE_ROOT).is_file():
        write_blocked(f"`{tree}` has no `{KERNEL_CRATE_ROOT.as_posix()}`")
        print(f"kernel_probe: `{tree}` is not a kernel tree with rust/kernel", file=sys.stderr)
        return 2
    if not args.moira.exists():
        print(f"kernel_probe: moira binary not found: {args.moira}", file=sys.stderr)
        return 2

    kernel_lib, samples = discover(tree)
    env = dict(os.environ, MOIRA_NO_CACHE="1")
    units: dict[str, dict] = {}
    assert kernel_lib is not None  # checked above
    units["kernel"] = probe_unit(args.moira, kernel_lib, [], env)
    for s in samples:
        units[f"samples/{s.name}"] = probe_unit(args.moira, s, [f"kernel={kernel_lib}"], env)
    for name, rec in units.items():
        status = "check ok" if rec["check_ok"] else ("parse ok" if rec["parse_ok"] else "parse fail")
        print(f"  {name}: {status} ({len(rec['diagnostics'])} diagnostic(s))", flush=True)

    rows = triage(units)
    summary = {
        "timestamp": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "units": len(units),
        "parse_ok": sum(1 for r in units.values() if r.get("parse_ok")),
        "check_ok": sum(1 for r in units.values() if r.get("check_ok")),
    }
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    (OUT_DIR / "results.json").write_text(json.dumps({"summary": summary, "units": units}, indent=2), encoding="utf-8")
    (OUT_DIR / "report.md").write_text(write_report(units, rows, summary, tree), encoding="utf-8")
    with (OUT_DIR / "history.jsonl").open("a", encoding="utf-8") as f:
        f.write(json.dumps(summary) + "\n")
    print(f"parse {summary['parse_ok']}/{summary['units']}, check {summary['check_ok']}/{summary['units']}")
    print(f"report: {OUT_DIR / 'report.md'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
