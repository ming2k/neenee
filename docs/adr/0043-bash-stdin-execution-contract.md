# 0043. Bash stdin execution contract: non-interactive by construction

- **Status:** Accepted
- **Date:** 2026-01-09

## Context

The `bash` tool spawned children with **stdin inherited from the parent
process** (`crates/neenee-tools/src/bash.rs`, pre-change: the spawn had no
`.stdin(...)` call). Because the harness runs inside a TUI that has taken the
terminal into raw mode + alt screen, the inherited stdin is the user's
keystrokes. A child that blocks on `read(stdin)` — `gpg`/`sudo`/`passwd`/a
`read` builtin, any `y/N` confirmation — therefore **waits for input that
never arrives and hangs silently** until the whole-command wall-clock timeout
(the default 30s) fires. There was no "alive but producing no output"
detection, and no recognition of known-interactive binaries.

This is a category error, not a missing feature: an autonomous agent produces
a frozen JSON `arguments` blob at call time and the agent loop has no
mechanism (and should have none) for the model to "type" into a running
process. So an interactive prompt is, *by definition*, unsatisfiable. The
correct architecture turns this into a fast, clear, diagnosable failure
rather than a silent hang — the same stance cron / CI runners / Ansible take
toward child processes.

A second, related problem: terminal control bytes in program output
corrupted the TUI layout. Investigation showed ANSI/CSI/OSC escapes
(including alt-screen `\e[?1049h` and OSC title sequences) were **already
stripped at capture** (`strip_ansi`). The real corruption came from
**carriage-return semantics**: capture is line-buffered on `\n`
(`BufReader::lines()`), so a `\r`-refreshed progress bar either never
emitted a line (no trailing `\n`) or was coarsely collapsed by the
renderer's `rsplit('\r').next()` (keeping only the last segment, losing a
short prefix that an earlier segment wrote past the later one's length).

Industrial prior art confirms the direction: Claude Code's bash tool
deliberately omits a `stdin` parameter from its schema
([anthropics/claude-code#64594]) and recommends `< /dev/null`; OpenCode's
issues ([anomalyco/opencode#18659], [#24885]) track the same stdin-blocks /
stall-watchdog needs as open work.

## Decision

A six-layer **execution contract** makes every shell step non-interactive by
construction, with control-sequence output normalized, and a human-input
escape hatch for the one legitimate interactive case (sudo/gpg passwords).

### Layer 1 — stdin closed by default (hard floor)

Spawn with `.stdin(Stdio::null())` unless a declared source provides input.
`Closed` is the only policy that is *correct by default* for an unattended
agent: a `read(stdin)` gets instant EOF and the command fails fast with a
real exit code, instead of hanging. Inner pipelines (`echo x | gpg …`) are
unaffected — that redirection is internal to `sh -c`.

### Layer 2 — dual timeout (no silent waiting)

Replace the single wall-clock timeout with two semantically distinct timers:
an **idle watchdog** (no stdout+stderr bytes for ~10s → assume stdin-blocked →
kill → `ShellTermination::IdleBlocked`) and the **wall-clock ceiling** (still
running too long → `Timeout`). Healthy long-running commands (build/test)
keep producing and reset the idle deadline; only a blocked command trips it.

### Layer 3 — interactive classifier (advisory)

`is_interactive_command(command)` matches the leading program token against
known-interactive binaries (`sudo`/`su`/`passwd`/`visudo`/`pinentry*`, bare
`gpg`, editors, pagers, live monitors). This is **advisory**: correctness
rests on Layers 1+2 (the hard floor catches anything the classifier misses),
so the classifier only makes failure *faster* and the error *more specific*.
`gpg --batch` / `gpg --passphrase*` is treated as non-interactive.

### Layer 4 — `\r`-aware capture

`normalize_carriage_returns(line)` resolves carriage-return / backspace
terminal semantics CI-log-viewer style: `\r` returns the caret to column 0
(without erasing), so the following text overwrites in place; `\b` steps one
column back; stray control bytes (BEL/FF/VT) are dropped; `\t` is preserved.
Applied at capture so neither the model-facing text nor the renderer ever
sees raw `\r`. The renderer's former `rsplit` approximation now delegates to
the same function, so both paths agree.

### Layer 5 — streaming alignment

The live-streaming seed (`push_tool_stream`) now builds real `ShellLine`
records (preserving the source-stream tag) instead of only flat
`stdout`/`stderr` strings, so the streaming view matches the final result:
stderr stays red-tinted and stdout/stderr keep their true arrival
interleaving, rather than the all-stdout-then-all-stderr degraded band.

### Layer 6 — themed termination footer

`ShellTermination` (`Exited`/`IdleBlocked`/`InteractiveBlocked`/`Timeout`/
`Cancelled`) drives a themed footer (`termination_footer`) so the user and
the model see *why* a command ended, not just that it did. Healthy `Exited`
is silent; every other variant paints a `warn()`/`err()` marker with a
non-interactive remedy hint. All colors flow through the shared theme tokens.

### Human/model input injection (the interactive escape hatch)

`StdinPolicy` is a first-class parameter on `Tool::call_structured_with_events`
(decided *before spawn* by the agent dispatch layer, never from the model's
JSON arguments):

- `Closed` — default hard floor.
- `Prefilled { data }` — a pipe provisioned in one of two declared-source
  situations:
  - **β (default): human input.** The classifier matched an interactive
    command; the operator supplied a response (e.g. a password) via an inline
    TUI panel (`Modal::InputInjection`, masked when `secret`). Round-trips
    through the same oneshot-park mechanism as `ask_user`.
  - **α (opt-in): model input.** Only when `[principal] allow_model_stdin` is
    `true` (default `false`) does the dispatch layer read a `stdin` argument
    the model supplied. In a default session stdin is *structurally
    unreachable* from the model's arguments — the security contract ("input
    may only come from a declared source") is enforced by the flag, not
    merely by schema omission.

The full round-trip mirrors `ask_user`/permission:
`AgentEvent::InputRequest` → TUI modal → `AgentRequest::InputReply` → parked
oneshot → `StdinPolicy::Prefilled` → spawn. Envoy routing is symmetric
(`EnvoyEvent::InputRequest`, `EnvoyHandle::reply_input`).

## Alternatives considered

**A PTY for every command.** Gives interactive programs a real TTY, but
re-introduces raw escape sequences (alt-screen, cursor moves) that must then
be stripped by a full VT100 state machine — spending PTY complexity *plus*
state-machine complexity to get output as clean as the existing
`strip_ansi`. Net benefit ~zero, and a PTY still can't make an *autonomous*
agent answer a password. Rejected. (A future `!cmd` opt-in host-shell mode
for *human*-driven interactive commands is a separate concern and not part
of this contract.)

**Mid-execution input injection.** Feeding the human's text into a child
that is *already running* would require reshaping the tool trait (a tool's
`call_structured_with_events` is a plain `.await` to completion with only
one-way `FnMut` callbacks). Pre-flight collection — gather the input before
spawn, then pipe it in — sidesteps the entire problem (the pipe buffer holds
the bytes ahead of the child's first read) without any trait surgery.
Chosen.

**A `stdin` JSON parameter always present, validated away.** Exposing stdin
in the model-writable schema and relying on validation/schema-suppression
to keep the model out of it is "defense by discipline." The flag-on-`Closed`
model is "defense by type": in a default session the model cannot supply
stdin at all. Rejected in favor of the structural guarantee, matching
Claude Code's deliberate schema omission ([anthropics/claude-code#64594]).

## Consequences

**Positive.** Interactive commands fail fast with a real exit code and a
diagnosable footer instead of hanging for 30s. Control sequences never
corrupt the layout. The streaming view matches the final result. The
non-interactive contract is structural, not conventional. `sudo`/`gpg`
passwords work via the human escape hatch without a PTY.

**Negative.** A command that legitimately needs interactive input *and* has
no non-interactive flag form cannot run unattended — it fails fast with a
remedy hint. This is the intended trade-off (and the state of every mature
agent harness). The idle budget (10s) very occasionally trips on a genuinely
slow-but-fine command; the footer tells the model to retry with a larger
timeout.

**Migration.** `ToolOutput::Shell` gains a `termination` field
(`#[serde(default)]` → restored sessions without it read as `Exited`).
`Tool::call_structured_with_events` gains a `stdin: StdinPolicy` parameter
(default `Closed`, non-breaking for tools that ignore stdin). New config:
`[principal] allow_model_stdin` (default `false`). Envoy profiles gain
`allow_model_stdin` (default `false` on every built-in).

## References

- ADR-0029 (full-duplex subagent communication) — the oneshot-park +
  up/down event round-trip this contract's input injection mirrors.
- ADR-0041 (tool capabilities, scope, and override) — `OperationScope` gates
  *whether* a command runs; the stdin contract governs *how its input is
  provisioned* once it does. Orthogonal.
- [anthropics/claude-code#64594] — Claude Code's bash tool inherits stdin by
  default; recommends `/dev/null`; no `stdin` parameter in the schema.
- [anthropics/claude-code#58938] — the `< /dev/null` outer-wrap convention.
- [anomalyco/opencode#18659] / [#24885] — OpenCode's stdin-blocks and
  stall-watchdog needs, tracked as open work.

[anthropics/claude-code#64594]: https://github.com/anthropics/claude-code/issues/64594
[anthropics/claude-code#58938]: https://github.com/anthropics/claude-code/issues/58938
[anomalyco/opencode#18659]: https://github.com/anomalyco/opencode/issues/18659
[anomalyco/opencode#24885]: https://github.com/anomalyco/opencode/issues/24885
