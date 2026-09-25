# Learnings Backlog — Reference-Repo Deep-Reads

Four comparison repos were read line-by-line by parallel agents (sessions 7-9),
producing 80 concrete findings. This file is the durable triage: what was
implemented where, what is roadmap, what was rejected and why. Update it when
an item ships.

Repos read: `tobi/disktree` (GPUI treemap analyzer), `Byron/dua-cli` (dua-core
4.1.0 work-stealing walker), `IgorMundstein/WinMemoryCleaner` (C# memory
cleaner), `vyrti/cleaner` (Rust system cleaner). Plus the best-practice
corpus (apollographql/rust-best-practices, actionbook/rust-skills, kvark's
optimization gist, JetBrains rewriting-in-rust, oneuptime memory guide).

## Implemented (with commit)

| Learning | Source | Landed |
|---|---|---|
| macOS error ladder: EINTR retry, EACCES→list-only attrs, ENOTSUP-class→std fallback (du-parity sizing) | dua-cli macos/mod.rs | `5f5937e` |
| Quickwins catalog invariant tests (never-clean roots, no bare env roots, single components, known ids, cache-leaf-only browsers, VM review-only, cap) | cleaner sysclean/tests.rs | `418aa91` |
| Saturating size sums in category rollup (property-wave convention) | property-wave precedent | `418aa91` |
| Platform monolith split into cohesive submodules + local cross-check harness | general hygiene (disktree module discipline) | PR #1 `3b2c5ee` |
| Win/mac FFI visibility + explicit per-file imports after the split | split fallout | PR #1 (CI rounds) |
| macOS bulk-parser E2E asserts sub/ names against the parent listing (test born broken) | found by CI matrix | `96c08ce` |

## Roadmap — engine (features, NOT refactoring; do in feature waves)

1. **Known-subtree memoization for widening rescan** (disktree `scan.rs:159-313`):
   hand the surviving `Arc` subtree to the next scan keyed by canonical path;
   `~` → `/` reads only what is outside `~`. Windows: memoize by parent MFT
   segment when USN/sequence numbers are unchanged. Pairs with path-keyed
   staging survival.
2. **Dense `DirectoryId` parenting** (dua-cli lib.rs:179-197): walker assigns
   dense ids at discovery; tree builder maps id→node via `Vec<Option<u32>>`
   and parents resolve by index — removes per-entry path hashing in tree
   construction. Largest structural simplification candidate in the core.
3. **IO-bound probe with parallel-stat fallback** (dua-cli 4.1.0, macos/mod.rs:256-323):
   when two refills spend more time waiting than executing, reopen with plain
   enumeration and distribute stats as parallel jobs (the 1M-file rustc
   `target/` case).
4. **`Order::ParentFirst` vs `Completion` publish modes** (dua-cli lib.rs:1399-1440):
   same pool, two orderings — streaming tree construction vs max-throughput totals.
5. **Bounded inline-degradation for queued work** (dua-cli lib.rs:1241-1264):
   at most 64 queued stat jobs per worker; the 65th runs inline. Apply to
   turbo MFT pending-directory queue and Tauri IPC event queues.
6. **Steal-ramp-up + CAS-claimed idle wake** (dua-cli lib.rs:717-734, 917-922):
   wake one worker per successful steal. macOS caps threads at 8 for exactly
   this reason (dua options.rs:63-69).
7. **`retain_depth`: aggregate everything, store N levels** (dua-cli traverse.rs:1155-1161):
   treemap/sunburst at fixed zoom does not need deep nodes in memory.
8. **Two-phase parallel delete + ticket-based unlink pool** (dua-cli deletion.rs:137-245):
   type-only walk → ticket pool → deepest-first rmdir batch; ENOTFOUND =
   success. Measured 0-11.5% faster, 40% shorter walks.
9. **Root entries enumerated once, reused as prepared walk roots** (dua-cli
   aggregate.rs:122-144): the UI's top-level enumeration hands entries to the
   engine so roots are never stat'ed twice.

## Roadmap — safety/product (feature waves)

10. **Path-keyed staging with absorption semantics** (disktree marks.rs):
    staged entries survive rescans/surgery by OS path, re-priced from the
    fresh tree, vanished entries zeroed not dropped; staging a directory
    absorbs staged children ("the saving is never counted twice").
11. **Plan phase refuse-gauntlet before commit** (disktree removal.rs:93-248):
    lexical normalize before guards, SYSTEM_TREES equivalents refused even
    where permissions allow, mount-point detection by device-vs-parent,
    outermost-dedup with covered/blocked tallies.
12. **Tier model for quickwins** (cleaner sysclean): Safe/Reclaimable/
    Destructive/NeedsRoot with only Safe default-selected; typed `WIPE`
    confirmation for destructive; `ReportOnly` rows sized but not selectable.
13. **Tool-first actions** (cleaner catalog): `docker system prune`,
    `npm cache clean`, `dotnet nuget locals --clear` instead of rm-ing
    managed caches; `requires` PATH probe gates row visibility.
14. **Residual cache sweep** (cleaner catalog/mod.rs:382-424): unknown dirs
    in cache roots ≥100 MiB become "Other" rows minus explicit-rule overlap.
15. **Empty vs Remove commit modes** (cleaner): keep the directory, delete
    contents — for dirs apps expect to exist (DerivedData, Temp, browser roots).
16. **Elevation as deferred reviewable handoff** (cleaner elevate.rs:170-252):
    system-junk rules build a readable PowerShell script + UAC, never silent.
17. **Logon autostart via schtasks XML** (WinMemoryCleaner App.xaml.cs:638-746):
    `RunLevel=HighestAvailable` + `InteractiveToken` + `ExecutionTimeLimit=PT0S`
    — eliminates per-boot UAC prompts for elevated apps.
18. **Self-deprioritization during scans** (WinMemoryCleaner App.xaml.cs:773-863):
    scan worker threads at Low priority (process class + per-thread +
    PriorityBoost off) so background scans never starve the shell.
19. **Resume-from-sleep reinit** (WinMemoryCleaner, fixed their crash #145):
    monitor interfaces/handles vanish across sleep; re-probe on resume with
    bounded retries instead of serving stale data.
20. **Honest free-space accounting** (disktree invariant #9): the Done screen
    shows the MEASURED statvfs/GetDiskFreeSpaceEx delta, not the sum of
    staged bytes.
21. **Cooldown floors on automated triggers** (WinMemoryCleaner): any
    auto-cleanup trigger needs a minimum-interval floor (theirs: 5 min).
22. **Tray icon as live telemetry** (WinMemoryCleaner NotificationService):
    usage % with threshold colors + scanning animation; 63-char tooltip
    clamp; icon-handle hygiene.
23. **Dual metric (bytes vs files) end-to-end** (disktree tree.rs): ranking,
    treemap areas and staging all take Metric; `node_modules` vs one ISO.
24. **Filter with `Keep::Partial` re-weighting** (disktree filter.rs): live
    typeahead where ancestors show at matched-beneath size.
25. **Layout cache key + base-space/view-transform split** (disktree state.rs):
    zoom/pan never re-layout; `zoomed_at` pins the cursor point.
26. **Header-band reservation + reverse hit-test + Others-tail** (disktree
    treemap.rs:187-317): parent-first emission, area-accurate tail merge.
27. **Snapshot/journey test culture** (dua-cli): streaming snapshot codec,
    insta golden CLI snapshots, scripted TUI journeys.
28. **Version-drift release gate** (WinMemoryCleaner release.yml): CI fails
    if the tag ≠ Cargo.toml/tauri.conf version; tests run from the shipped
    binary.
29. **AV false-positive playbook** (WinMemoryCleaner): signing + automated
    VirusTotal/Hybrid-Analysis submission + documented hash verification —
    an elevated Rust binary touching raw volumes WILL get flagged.
30. **`--reset` kill switch** (WinMemoryCleaner): survives corrupted
    settings, disables auto-update to break crash loops.
31. **Structured JSON Event Log channel** (WinMemoryCleaner Logger): the
    elevated side can't ship logs to a webview; Event Log with JSON payloads
    is the auditable telemetry-free trail.
32. **Single-instance foreground dance** (WinMemoryCleaner App.xaml.cs:443-464):
    `AllowSetForegroundWindow` so double-launch shows the existing window.
33. **Sidecar localization overrides** (WinMemoryCleaner Localizer):
    translators PR a file next to the exe, not a build.
34. **Browser multi-profile globs** (cleaner): `User Data/*/Cache` — all
    profiles, not just Default (DiskBytes has this; keep the constraint
    comment test-pinned).

## Rejected (with reason)

- **Full rescan after delete instead of surgery** (disktree): deliberate
  simplicity trade; DiskBytes' surgery is better and now regression-tested.
- **`own_bytes` derived-only aggregation** (disktree): breaks the streaming
  live-totals property DiskBytes' scanner is built around; DiskBytes
  aggregates during scan by design.
- **Permanent delete only, no recycle** (cleaner): DiskBytes' recycle-bin
  commit model is strictly safer; keep.
- **Killing taskmgr/mmc to uninstall** (WinMemoryCleaner): far too
  aggressive; never adopt.
- **Tests compiled into the shipped binary** (WinMemoryCleaner): bloat and
  attack surface; never adopt.
- **256 KiB → 64 KiB buffer shrink** (dua-cli): DiskBytes' 256 KiB is a
  documented BuildPrompt §4 choice; revisit only with A/B benchmarks on
  real volumes (criterion suite now exists to measure it).

## Hygiene policies adopted going forward

- Lint posture (disktree): every `#[allow]` carries a one-line rationale;
  toolchain pinned so lint changes are deliberate, not surprises.
- Progress = relaxed atomics + snapshot + capped error detail (disktree);
  never channel per-entry events to the UI.
- Epoch counters invalidate every async result (disktree); stale results
  dropped on arrival. (DiskBytes has generation authority — keep the
  pattern for every new async surface.)
- Error aggregation over protected objects: ERROR_ACCESS_DENIED on Windows
  and EACCES on macOS are expected noise, never surfaced as errors.
