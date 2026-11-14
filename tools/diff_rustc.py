#!/usr/bin/env python3
"""Differential check: Moira's C backend against rustc, on the same programs.

For each `.rs` file given (or every file in a directory):

  1. rustc builds the program and runs it; its stdout is the reference.
  2. `moira --emit c` lowers it, `cc` builds the C, and that binary runs.
  3. The two stdouts are compared (line endings normalised).

Programs that use Moira-only builtins (`print_i32`, ...) are not valid rustc input.
`--shim "<rust source>"` prepends Rust definitions for the rustc side only, so such a
program can still be compared. A program rustc rejects is reported as `rustc-rejects`
and is not counted as a Moira failure; it means the program is not valid Rust.

Outputs go to a temp directory (removed on exit unless `--keep`).

Usage:
  python tools/diff_rustc.py demos/native_demo.rs --shim "fn print_i32(x: i32) { println!(\"{}\", x); }"
  python tools/diff_rustc.py path/to/dir
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(args: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(args, capture_output=True, text=True, **kw)


def normalise(text: str) -> str:
    return text.replace("\r\n", "\n")


def compare(src: Path, moira: Path, cc: str, work: Path, shim: str | None) -> tuple[str, str]:
    """Return (status, detail). Status is `same`, `diff`, `rustc-rejects`, or `moira-fails`."""
    name = src.stem
    rust_src = src
    if shim:
        rust_src = work / f"{name}_shim.rs"
        rust_src.write_text(shim + "\n" + src.read_text(encoding="utf-8"), encoding="utf-8")
    rust_exe = work / f"{name}_rust.exe"
    built = run(["rustc", "-A", "warnings", "--edition", "2021", "-o", str(rust_exe), str(rust_src)])
    if built.returncode != 0:
        return "rustc-rejects", built.stderr.strip().splitlines()[0] if built.stderr.strip() else "rustc failed"
    rust_out = run([str(rust_exe)])
    expected = normalise(rust_out.stdout + (f"rc={rust_out.returncode}\n"))

    c_file = work / f"{name}.c"
    emitted = run([str(moira), "--emit", "c", "-o", str(c_file), str(src)])
    if emitted.returncode != 0 or not c_file.exists():
        msg = (emitted.stderr.strip().splitlines() or ["no output"])[-1]
        return "moira-fails", msg
    moira_exe = work / f"{name}_moira.exe"
    built_c = run([cc, "-w", str(c_file), "-o", str(moira_exe)])
    if built_c.returncode != 0:
        return "moira-fails", "C does not build: " + (built_c.stderr.strip().splitlines() or ["?"])[0]
    moira_out = run([str(moira_exe)])
    actual = normalise(moira_out.stdout + (f"rc={moira_out.returncode}\n"))
    if actual == expected:
        return "same", ""
    return "diff", f"rustc: {expected!r}\n  moira: {actual!r}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("paths", nargs="+", type=Path, help=".rs files or directories")
    ap.add_argument("--shim", default=None, help="Rust source prepended on the rustc side only")
    ap.add_argument("--moira", type=Path, default=ROOT / "target" / "debug" / ("moira.exe" if os.name == "nt" else "moira"))
    ap.add_argument("--keep", action="store_true", help="keep the work directory")
    args = ap.parse_args()

    if shutil.which("rustc") is None:
        print("diff_rustc: rustc not found", file=sys.stderr)
        return 2
    cc = next((c for c in ("gcc", "clang", "cc") if shutil.which(c)), None)
    if cc is None:
        print("diff_rustc: no C compiler found", file=sys.stderr)
        return 2
    if not args.moira.exists():
        print(f"diff_rustc: moira binary not found: {args.moira}", file=sys.stderr)
        return 2

    files: list[Path] = []
    for p in args.paths:
        files += sorted(p.glob("*.rs")) if p.is_dir() else [p]

    work = Path(tempfile.mkdtemp(prefix="moira_diff_"))
    counts = {"same": 0, "diff": 0, "rustc-rejects": 0, "moira-fails": 0}
    try:
        for f in files:
            status, detail = compare(f, args.moira, cc, work, args.shim)
            counts[status] += 1
            print(f"{status:>14}  {f}" + (f"\n                {detail}" if detail and status != "same" else ""))
    finally:
        if args.keep:
            print(f"work directory kept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)
    print("summary: " + ", ".join(f"{k} {v}" for k, v in counts.items()))
    return 1 if counts["diff"] or counts["moira-fails"] else 0


if __name__ == "__main__":
    sys.exit(main())
