# 0018. Per-project multi-instance concurrency

- **Status:** Accepted
- **Date:** 2026-06-24

## Context

ADR-0005 §3 declared `neenee-store` the **local coding-agent** persistence
layer and codified one of its assumptions as:

> A process-level `flock` enforces single-instance-per-project.

That assumption is enforced in `crates/neenee-cli/src/main.rs:132-138`,
which acquires `lock::ProcessLock` on
`paths::get().project_lock_file(&project_root)` for every startup except
`doctor`. The lock itself lives in `crates/neenee-store/src/lock.rs:33`.

The assumption exists for a concrete reason, not for user convenience: the
store was built around **one active session per project**, and two writers
on that single session corrupt each other. The races are in the code, not
just in the docs:

- **`EventLog::append` does load-then-write for `seq`.**
  `crates/neenee-store/src/events.rs:121-142` reads
  `self.load()?.last().seq + 1` and then appends. Two processes can both
  read `seq=N` and both write `seq=N+1`; `EventLog::load` then sorts by
  `seq` (`events.rs:114`), so replay order becomes undefined.
- **`session.json` is both the active-session pointer and a cache
  snapshot, rewritten on every mutation.** `SessionStore::load_for_project`
  rewrites it on open (`session.rs:468`); `replace_messages`
  (`session.rs:630`), `set_checkpoint` (`session.rs:641`),
  `commit_compaction` (`session.rs:656`), and `reset` (`session.rs:678`)
  all call `self.persist(&data)` → `atomic_write_json` → `rename(2)`.
  Each write is atomic, but the pattern is last-write-wins across
  processes: whichever instance saves second erases the other's snapshot.
- **`data: Mutex<SessionData>` is process-local** (`session.rs:475`). It
  serializes turns *within* one process and does nothing across processes,
  so two instances hold two independent copies that diverge and then
  clobber each other through the shared snapshot.
- **`reset()` reaches across instances.** Instance A starts a fresh
  session (`StartupMode::Fresh` → `session.reset()` at `session.rs:659`)
  while instance B is mid-turn; B's next `persist` writes its stale
  in-memory `data` back and silently undoes A's reset.

The same read-modify-write shape recurs on shared **global** state outside
the session: per-model telemetry (`provider_usage.rs:40` / `:61`),
slash-command history (`config.rs:263` / `:272`), and the per-project
embedding index (`embedding.rs:205`). Content-addressed blobs are the one
shared store that is already concurrency-safe, because dedup by hash makes
concurrent writes of the same content idempotent.

Meanwhile, ADR-0017 already moves the codebase toward **one store = one
session = one file**: `SessionStore::fork_to_side` writes a self-contained
side file under `<project>/sessions/<side_id>.json` with its own
`events.jsonl`, and the side `LiveSession` is built with
`SessionStore::for_path` (`session.rs:479`). That pattern is proven but
capped at one in-process side.

The user-facing ask is simple: running two `neenee` processes in the same
project — parallel code review, an agent run alongside interactive chat —
should work. The question is what minimal structural change lets that
happen without losing data.

## Decision

Retire the single-active-session assumption. Give every live instance its
own session files in the project bucket, and put short file-scoped locks
around the handful of shared global read-modify-write paths. Three pillars:

1. **Drop the per-project `ProcessLock`.** Remove the acquire in
   `main.rs:132-138` for normal startup (the `doctor` branch already
   skips it and keeps doing so). The lock file itself can stay as a
   best-effort mutual-exclusion escape hatch for users who want the old
   behaviour, exposed behind an opt-in flag; the default becomes
   unlocked.

2. **One session file per live instance.** Every running `neenee` in a
   project pins its own `sessions/<id>.json` plus
   `sessions/<id>.events.jsonl` inside the project bucket — exactly the
   self-contained-file layout ADR-0017's `fork_to_side` introduces for
   sides. Concretely:

   - `StartupMode::Fresh` mints a new `<id>` and opens
     `SessionStore::for_path` on `sessions/<id>.json`, instead of
     resetting a shared `session.json`.
   - `StartupMode::Picker` / `StartupMode::Resume(id)` open an existing
     `sessions/<id>.json`. `/sessions` lists every file in the bucket.
   - The project-root `session.json` and `events.jsonl` are retired.
     "Active session" stops being a shared on-disk pointer; it is purely
     a per-process choice, so no two processes ever write the same file.
     The `session.json` snapshot race and the `EventLog::append` seq
     race both disappear by construction, because each log has exactly
     one writer.

   This generalizes ADR-0017's invariant from "a side is a
   self-contained file" to "every live session is a self-contained
   file." `fork`, `fork_to_side`, and `Fresh` become the three ways to
   mint a session file; the underlying store does not distinguish them.

3. **File-scoped locks on the remaining shared global read-modify-write
   paths.** Wrap `load → mutate → save` with `flock(LOCK_EX)` on the
   file's own file descriptor (held only for the short RMW window,
   released before any user-visible work). Apply this to:

   - `provider_usage.json` (`provider_usage.rs:40` / `:61`),
   - slash-command `history.json` (`config.rs:263` / `:272`),
   - the per-project embedding index (`embedding.rs:205`).

   Blobs need no lock (content-addressed, idempotent). A per-project
   **migration lock** guards the one-time legacy `session.json` →
   `sessions/<id>.json` move so two first-time instances do not race the
   migration.

## Alternatives considered

- **Keep the flock, add a `--no-lock` escape hatch.** Rejected: the flock
  exists because concurrent writers corrupt state. An escape hatch ships
  the corruption with a warning label and teaches users to ignore the
  warning. The real fix is to remove the shared mutable file, which pillar
  2 does.

- **Server-assigned `seq` via a long-lived broker process.** Rejected as
  over-engineered for a local single-user tool. Pillar 2 removes the
  shared event log entirely, so there is no central `seq` to assign.

- **Refactor `SessionStore` to operate-by-id over one shared log.**
  Rejected for the same reason ADR-0017 rejected it: large API churn
  across the turn hot path, and interleaving two conversations' events
  in one log produces a Frankenstein session on replay. One-file-per-
  session avoids both the API churn and the semantic mess.

- **ADR-0017 in-process sides only; do not support cross-process.**
  Rejected because it leaves the user unable to run two `neenee`
  processes in one project, which is the ask. The two ADRs compose: the
  same self-contained-file invariant serves in-process sides and
  cross-process instances, and ADR-0017's `fork_to_side` becomes one of
  three ways to mint a session file.

- **Keep the shared active pointer, add a "who is active" coordinator
  lock.** Rejected: it serializes the two instances through one pointer
  and re-introduces the clobber race the moment the lock holder crashes.
  Per-instance files are crash-isolated by construction — a crash mid-turn
  never touches another instance's files.

## Consequences

Positive:

- **Multiple `neenee` instances in one project, by construction.** Each
  writes only its own `sessions/<id>.*`; a crash mid-turn never corrupts
  another instance.
- **Composes with ADR-0017.** "One store = one session = one file"
  becomes the universal invariant. The `/sessions` picker, `fork`,
  `/btw` sides, and parallel cross-process instances are all the same
  mechanism.
- **The snapshot and `seq` races are gone**, because no file has two
  writers. The snapshot becomes a pure rebuild cache; losing it regenerates
  from that instance's event log.
- **Removes a process-wide lock that surprised users** — the original
  "another neenee instance may already be running" error stops firing on
  the default path.

Negative:

- **One-time migration** of legacy project-root `session.json` /
  `events.jsonl` into `sessions/<legacy-id>.json` /
  `sessions/<legacy-id>.events.jsonl` on first open, guarded by a
  per-project migration lock. `/doctor` gains a check that every
  `sessions/*.json` has a matching `events.jsonl`.
- **On non-Unix platforms the file-scoped locks in pillar 3 are no-ops**
  (`lock.rs:54-58` is already a no-op there), so the global RMW races on
  `provider_usage` / slash history remain. Acceptable because that state
  is rebuildable cosmetic telemetry, never conversation data; documented
  in `docs/explanation/persistence.md`.
- **Disk usage grows with live + archived sessions** in a project.
  `/sessions` gains archive/prune guidance. This is partly true today via
  archived sessions; multi-instance makes it more visible.
- **"What is the active session for this project" loses its single
  answer.** The startup picker and `/resume` become the only entry
  points. Intentional: the active pointer was the race.

Migration:

- Legacy project-root `session.json` / `events.jsonl` move into
  `sessions/<existing-id>.*` on first open under the new code, under the
  per-project migration lock.
- `/doctor` verifies the migration and that every `sessions/*.json` has a
  matching `events.jsonl`.
- Session and event-log schema versions bump together so mixed-version
  instances detect the split cleanly.

## References

- [ADR-0005](0005-strict-layering-and-renames.md) §3 — the
  single-instance-per-project assumption this ADR relaxes.
- [ADR-0014](0014-xdg-persistence-architecture.md) — the project bucket
  layout `sessions/<id>.*` lives inside.
- [ADR-0017](0017-side-conversations.md) — self-contained side files;
  this ADR generalizes the pattern to every live session.
- `crates/neenee-store/src/lock.rs` — the `ProcessLock` being retired
  from the default path.
- `crates/neenee-store/src/events.rs` — `EventLog::append` `seq` race
  (lines 121-142).
- `crates/neenee-store/src/session.rs` — the active pointer / snapshot
  writes that pillar 2 removes (`for_path` at line 479, `persist`
  call sites at 468 / 630 / 641 / 656 / 678).
- `crates/neenee-store/src/provider_usage.rs`,
  `crates/neenee-store/src/config.rs`,
  `crates/neenee-store/src/embedding.rs` — the shared global RMW paths
  pillar 3 locks.
