# Hint line

Single-row status strip below the input box. The right side carries the
model name and context-usage indicator; the left side carries a flat
`UNATTENDED` label (warning tone) while write-tool prompts are bypassed,
and a `[ SHELL ]` pill only when a `!`-prefixed shell command is staged
in the prompt.

## Appearance

Normal chat (no labels on the left):

```text
                            Kimi K2.7 Code   89.2k (8%)
```

Unattended mode active (`UNATTENDED` flag on the left):

```text
  UNATTENDED                Kimi K2.7 Code   89.2k (8%)
```

With a `!`-prefixed shell command staged:

```text
  [ SHELL ]                 Kimi K2.7 Code   89.2k (8%)
```

Both at once (unattended + staged shell command):

```text
  UNATTENDED  [ SHELL ]     Kimi K2.7 Code   89.2k (8%)
```

There is no compose/browse mode pill: the TUI has a single navigation
state, not two zones (see [Transcript focus](#transcript-focus) below).
When a transcript step carries keyboard focus, the focused step itself is
reverse-highlighted in the transcript — the hint line does not advertise
it.

| Attribute | Value |
|-----------|-------|
| Location | 1 row below the input box |
| Unattended flag | `UNATTENDED` (warning tone + BOLD), only while write-tool prompts are bypassed |
| Shell pill | `[ SHELL ]` (warning tone + raised bg), only while the prompt is `!`-prefixed |
| Model name | `brand` + BOLD |
| Context usage | `89.2k` in `text_muted`; `(8%)` in threshold color (green/yellow/red) |
| Background | `surface` |

## Unattended mode

When unattended mode is active (`--unattended` / `/unattended on`), the
hint bar shows a flat `UNATTENDED` label in the warning tone at its left
edge. Plain text rather than a bracketed pill: it reads as a persistent
state flag (always-on while the session is elevated) rather than a
momentary input mode, so it carries its meaning without any chrome. The
composer's `›` prompt glyph keeps its usual brand color — the elevated
state lives entirely on the hint line.

## Transcript focus

There are no focus *zones* and no zone-toggle key. A single optional
focused step (`App::focused_target`) is the only navigation state:

| Key | Effect |
|-----|--------|
| `Ctrl+↑` / `Ctrl+↓` | Focus / cycle the nearest transcript step |
| `↑` / `↓` (while focused) | Cycle to the previous / next step |
| `Enter` (while focused) | Open the focused step |
| `Esc` (while focused) | Clear the focus |

While a step is focused the composer panel drops to its dimmer palette to
signal that the next key acts on the step, not the input. Typing any
printable character still lands in the prompt — there is no mode that
captures typing. `Tab` is **not** a focus toggle; it only accepts a
completion suggestion when one is open.

## Visibility

Hidden when overlay modals are open.

## Source

`draw_hint_bar` / `HintBarView` in `render/chrome.rs`. The focused-step
palette switch lives in `draw_composer` (`render/composer.rs`); the
`Ctrl+↑`/`Ctrl+↓` handling lives in `input/mod.rs`.
