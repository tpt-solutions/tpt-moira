# Moira Stability & Versioning Policy

This document defines Moira's stability guarantees and versioning policy. It is
the authoritative reference for what users can rely on across releases and how
the project treats divergences from `rustc` behavior. It is part of the Phase-4
production-track work (see `TODO.md`).

## 1. Scope

Moira is a from-scratch Rust compiler. Because it does **not** yet implement the
full Rust language (see `docs/SELF_HOSTING.md`), this policy distinguishes
between:

* **The supported subset** — the constructs Moira accepts and compiles today
  (documented by the `lachesis/tests` acceptance slices and the Phase-1/2/3
  feature checkboxes in `TODO.md`).
* **Everything else** — constructs Moira currently rejects or mis-compiles.

Stability guarantees below apply **only to the supported subset**. Behavior on
unsupported constructs is undefined and may change without notice.

## 2. Versioning

Moira uses **Semantic Versioning** (`MAJOR.MINOR.PATCH`), with the pre-1.0
caveat below.

* **`0.y.z` (current era, until 1.0):** the compiler is explicitly unstable.
  * `PATCH` (`0.y.Z`): bug fixes, performance, and docs that do not change the
    supported subset or the documented CLI/artifact formats.
  * `MINOR` (`0.Y.0`): additive growth of the supported subset, new `--emit`
    artifacts, new flags (non-breaking), and behavioral fixes that bring Moira
    **closer** to `rustc` on already-supported constructs. A `MINOR` bump may
    change the *content* of emitted artifacts (MIR/SSA/C/ASM text) as long as
    the observable program behavior is unchanged or improved.
  * The **supported subset may expand** in any `0.y` release; this is the
    primary signal of progress and is not considered a breaking change.
* **`1.0.0` and beyond:** the supported subset is frozen as a stable contract.
  Breaking changes to the supported subset, CLI surface, or on-disk cache format
  require a `MAJOR` bump with a deprecation window (Section 4).

### 2.1 CLI surface

The following are part of the public CLI contract once Moira reaches 1.0:

* `moira --emit {ast,mir,ssa,c,asm,native,check}` — the set of emit modes and
  their meaning.
* `--check`, `--diff`, `--parity`, `--propose`, `--allowlist`,
  `--update-allowlist` — the check/diagnostic flags.
* `-o <file>` — output redirection.

**Stability rule:** removing an `--emit` mode or changing its *semantic meaning*
is a breaking change (requires `MAJOR`). Adding a new mode or flag is
non-breaking. Flag *spellings* are stable; short aliases may be added but never
removed without a `MAJOR` bump.

### 2.2 On-disk incremental cache

The content-addressed cache under `$TEMP/moira_cache` (and any future stable
location) is an **implementation detail, not a public format**. Its layout may
change in any release; Moira tolerates a missing or stale cache (it recomputes).
A stable, documented cache format (e.g. for distributed/CI caching) is a future
`1.0` deliverable and will be versioned independently with a format header.

## 3. Divergence from `rustc` (the triage-queue policy)

Moira intentionally diverges from `rustc` on unsupported constructs. Divergence
on the **supported subset** (where `rustc` and Moira disagree on a program both
accept) is a **bug**, not a feature, and is tracked through the
`moira --diff` harness.

### 3.1 How long may a divergence live?

* **Crash / panic in Moira** on a supported-subset program that `rustc`
  accepts: treated as a **release blocker** — must be fixed before the next
  `MINOR` (ideally `PATCH`).
* **Wrong code / mis-compile** on the supported subset (program compiles but
  behaves differently from `rustc`): **release blocker**, same as above.
* **Rejected-but-should-accept** (Moira rejects a supported-subset program
  `rustc` accepts): tracked as a known divergence in the allowlist
  (`.moira-divergence-allowlist`); must be fixed before the next `MINOR`.
* **Diagnostic wording/quality** differences: non-blocking; continuously
  improved via the parity audit (`moira --diff --parity`) and the triage queue
  (`.moira-triage.jsonl`). A divergence that only affects *message text* may
  persist across multiple `MINOR` releases while the underlying accept/reject
  behavior matches.

Known, documented divergences that are **outside** the supported subset are
recorded in `docs/SELF_HOSTING.md` and are not subject to this policy.

### 3.2 Allowlist vs. fixes

The divergence allowlist is a **triaging aid**, not a way to permanently bless a
divergence on the supported subset. Allowlist entries for supported-subset
programs must carry a tracking issue/label and a target `MINOR` by which they
will be fixed or promoted to a documented, intentional behavior.

## 4. Breaking-change process & deprecation windows

Once Moira is 1.0:

1. A breaking change is proposed with a tracking issue referencing this policy.
2. The old behavior is **deprecated** (emits a warning / is documented) in one
   `MINOR` release.
3. The new behavior becomes default, and the old is removed, no sooner than the
   **next `MINOR`** (minimum one release cycle of overlap).
4. Removal is noted in the changelog generated by `x.py changelog` (Section 5).

Until 1.0, breaking changes may land in any `0.y` release without a deprecation
window, but they must be called out in the release changelog.

## 5. Release cadence

Moira uses a **milestone-based** cadence aligned with the phases in `spec.txt`
and the checkboxes in `TODO.md`:

* **Regular `MINOR` releases** are cut when the supported subset grows a
  meaningful step (e.g. a phase completes, a major feature lands) or on a
  roughly quarterly cadence, whichever comes first.
* **`PATCH` releases** are cut as needed for bug/perf fixes.
* **`MAJOR` (1.0)** is cut when the supported subset is frozen as a stable
  contract and the self-hosting + production-validation tracks
  (`TODO.md` Phase 4) are substantially met.

Every tagged release:

* builds release artifacts for the three OSes via `x.py dist` (gated on the
  Stage-2 cross-process determinism check),
* publishes a GitHub release with checksums and (when configured) signed
  artifacts,
* generates a changelog from commit history / PR labels via `x.py changelog`,
* and updates the benchmark dashboard (`moira-bench --dashboard`).

## 6. Relationship to other docs

* `docs/ARCHITECTURE.md` — how the pieces fit together.
* `docs/SELF_HOSTING.md` — the supported-subset growth plan and known gaps.
* `TODO.md` — the feature/phase checklist this policy governs.
* `CONTRIBUTING.md` — how to propose changes under this policy.
