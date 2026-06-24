//! Input-box completion pipeline: `/slash` commands and inline `@path` file
//! mentions. The pipeline is stateless on top of [`App::input`] — each
//! keystroke re-derives the candidates from the live text and the (cached)
//! recursive project scan.

use crate::startup::BuiltinCmd;
use crate::tui::App;

// The built-in slash-command vocabulary (names + descriptions) lives in ONE
// place: `startup::BuiltinCmd::ALL`. Completion, `/help`, and the dispatch
// `match` in `main.rs` all derive from it, and that dispatch is a
// non-exhaustive match over `Option<BuiltinCmd>` — so a command added to the
// table without a handler arm (or vice versa) fails to compile. There is no
// second list here to drift out of sync.

/// Kind of completion menu the input box is currently offering. Drives the
/// keyboard shortcuts that cycle / accept entries: Tab, ↑/↓, and (for slash
/// only) plain Enter on a unique prefix. Path mentions only complete via Tab
/// so a plain Enter still sends the message as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionKind {
    /// No completion menu is active.
    #[default]
    None,
    /// `/command` and subcommand completion (replaces the whole input).
    Slash,
    /// `@path` file mention completion (splices into the input at the cursor).
    Path,
}

/// A single completion candidate rendered in the completion menu. The
/// `replace_start..replace_end` byte range is the slice of the current input
/// that gets overwritten by `label` when the candidate is accepted, so slash
/// commands (which replace the whole input) and inline `@path` mentions
/// (which replace only the `@prefix` token) share one accept path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text to insert at the replace range.
    pub label: String,
    /// Hint shown to the right of the label (e.g. "Set pursuit", "dir", "1.2k").
    pub description: String,
    /// Byte offset in `App::input` where the replacement starts.
    pub replace_start: usize,
    /// Byte offset in `App::input` where the replacement ends.
    pub replace_end: usize,
}

impl Completion {
    /// Build a slash-command style completion that replaces the whole input
    /// (`replace_start = 0`, `replace_end = input_len`).
    fn whole_input(label: &str, description: &str, input_len: usize) -> Completion {
        Completion {
            label: label.to_string(),
            description: description.to_string(),
            replace_start: 0,
            replace_end: input_len,
        }
    }
}

/// Upper bound on the number of filesystem entries scanned for a single `@`
/// mention completion. Bounds the work on huge directories (e.g. generated
/// `node_modules`) so each keystroke stays imperceptible; the menu renders the
/// first six and cycles through the rest with ↑/↓.
const MAX_PATH_COMPLETIONS: usize = 200;

/// Cached recursive project listing for `@path` completion. Entries are
/// normalized to forward-slash paths relative to the captured cwd:
/// directories get a trailing `/`, files do not. Built once by
/// [`scan_project_files`] (ripgrep-first, manual walk fallback) and reused
/// across keystrokes, mirroring the per-directory picker cache in opencode's
/// TUI so each keystroke only filters instead of re-scanning.
#[derive(Debug, Clone)]
pub struct PathScan {
    pub entries: Vec<String>,
}

/// Recursively list files (and synthesized directory entries) under `cwd`,
/// respecting `.gitignore` and `.ignore`. Hidden files are included by
/// default so the user can mention e.g. `.env`; `.git/` is always excluded.
///
/// Prefers `rg --files` (fast, gitignore-aware, already a project dep) and
/// falls back to a manual recursive walk when `rg` is unavailable so the
/// feature still works on stripped systems. Matches the ripgrep-fallback
/// behaviour opencode uses when its native `fff` picker is missing.
pub(super) fn scan_project_files(cwd: &std::path::Path) -> PathScan {
    let entries = try_ripgrep_scan(cwd).unwrap_or_else(|| manual_walk(cwd));
    PathScan { entries }
}

/// Ripgrep-backed project scan. Returns `None` if `rg` cannot be spawned or
/// exits non-zero so the caller can fall back to [`manual_walk`].
fn try_ripgrep_scan(cwd: &std::path::Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("rg")
        .args([
            "--files",
            "--hidden",
            "--glob=!.git",
            "--color=never",
            "--no-messages",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect();

    // Synthesize directory entries by walking each file's ancestor chain—
    // `rg --files` only emits files, so directories are derived. Matches
    // opencode's ripgrep-fallback behaviour.
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in &files {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        // All but the last segment (the filename) are directory ancestors.
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.insert(format!("{}/", acc));
        }
    }

    let mut entries: Vec<String> = files;
    entries.extend(dirs);
    // Dirs first (alphabetic), then files (alphabetic). Case-insensitive to
    // keep `README.md` and `readme.md` adjacent on case-insensitive FSes.
    entries.sort_by(|a, b| {
        let a_dir = a.ends_with('/');
        let b_dir = b.ends_with('/');
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    entries.dedup();
    Some(entries)
}

/// Pure-Rust recursive directory walk used when `rg` is unavailable. Skips
/// `.git/` unconditionally; hidden files and other ignored directories are
/// included so users can still mention e.g. `.env` or `.github/workflows`.
pub(super) fn manual_walk(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_prefix)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = match entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            // `.git/` is always skipped to avoid dumping the entire repo
            // internals into the completion list.
            if name == ".git" {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let rel = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}{}", rel_prefix, name)
            };
            if is_dir {
                let child_rel = format!("{}/", rel);
                stack.push((entry.path(), child_rel.clone()));
                out.push(child_rel);
            } else {
                out.push(rel);
            }
        }
    }
    out.sort_by(|a, b| {
        let a_dir = a.ends_with('/');
        let b_dir = b.ends_with('/');
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    out
}

/// Decide whether a cached path entry should be shown for a given `@query`.
///
/// - Empty query: only top-level entries (immediate children of cwd), so the
///   initial menu is a small, useful overview instead of every nested file.
/// - Query without `/`: case-insensitive substring match anywhere in the
///   path, so `@foo` finds `src/foo.rs` and `Cargo.lock` alike.
/// - Query ending in `/` (e.g. `@src/`): case-insensitive prefix match,
///   listing that directory's descendants so the user can descend naturally.
/// - Other queries: case-insensitive substring match — covers `@src/foo` and
///   similar mid-path fragments.
pub(super) fn path_query_match(path: &str, query: &str) -> bool {
    if query.is_empty() {
        // Top-level: a path with no `/`, or a single trailing `/` and nothing
        // else (top-level directory).
        let trimmed = path.trim_end_matches('/');
        !trimmed.contains('/')
    } else if let Some(dir_prefix) = query.strip_suffix('/').filter(|_| query.contains('/')) {
        // Query is `@<dir>/`: descend, prefix match.
        path.to_lowercase().starts_with(&dir_prefix.to_lowercase())
    } else {
        path.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Pure core of [`App::active_mention_range`]. Given the input bytes and a
/// byte offset sitting at the caret, return the inclusive `(start, end)` range
/// of the `@mention` token the caret is inside, or `None` when no token is
/// active. See the method docs for the rules.
pub(super) fn mention_range_at(input: &str, cursor_byte: usize) -> Option<(usize, usize)> {
    if cursor_byte > input.len() {
        return None;
    }
    let before = &input[..cursor_byte];
    // Walk back over chars from the cursor looking for an `@` without
    // crossing whitespace. `char_indices` gives byte offsets so the range we
    // return can be sliced straight out of the input.
    let mut chars_before: Vec<(usize, char)> = before.char_indices().collect();
    while let Some((idx, c)) = chars_before.pop() {
        if c.is_whitespace() {
            return None;
        }
        if c == '@' {
            let preceding_whitespace = chars_before
                .last()
                .map(|(_, prev_c)| prev_c.is_whitespace())
                .unwrap_or(true);
            return if preceding_whitespace {
                Some((idx, cursor_byte))
            } else {
                None
            };
        }
    }
    None
}

impl App {
    /// Classify which completion menu, if any, should be shown for the current
    /// input + cursor state. Slash commands take priority over `@path` mentions
    /// because a slash input is a command-in-progress and never carries inline
    /// file references.
    pub fn completion_kind(&self) -> CompletionKind {
        if self.input.starts_with('/') {
            CompletionKind::Slash
        } else if self.active_mention_range().is_some() {
            CompletionKind::Path
        } else {
            CompletionKind::None
        }
    }

    /// Compute the live completion candidates for the current input + cursor.
    /// Returns an empty `Vec` when no menu should be shown. See [`Completion`]
    /// for the slash-vs-path replace-range semantics. Takes `&mut self` so the
    /// `@path` scan can populate [`App::path_scan_cache`] on first use.
    pub fn completions(&mut self) -> Vec<Completion> {
        let current = self.input.to_lowercase();

        // Subcommand completion for /mode
        if let Some(after) = current.strip_prefix("/mode ") {
            return [
                ("/mode build", "Build mode — full read/write tool access"),
                (
                    "/mode plan",
                    "Plan mode — read-only tools, safe exploration",
                ),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/mode ")
                    .map(|sub| sub.to_lowercase().starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/pursue ") {
            return [
                ("/pursue status", "Show the current pursuit"),
                ("/pursue stop", "Stop the active pursuit"),
                ("/pursue done", "Mark the pursuit completed"),
                ("/pursue clear", "Remove the pursuit"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/pursue ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/permissions ") {
            return [(
                "/permissions clear",
                "Clear process-local always-allow rules",
            )]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/permissions ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/session ") {
            return [
                ("/session status", "Show session id and loop checkpoint"),
                ("/session list", "List durable session branches"),
                (
                    "/session resume",
                    "Resume the most recent or selected session",
                ),
                ("/session fork", "Fork the current conversation"),
                ("/session new", "Start a new durable session"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/session ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if current.starts_with('/') {
            return BuiltinCmd::ALL
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&current))
                .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
                .chain(self.custom_commands.iter().filter_map(|(command, desc)| {
                    if command.starts_with(&current) {
                        Some(Completion::whole_input(
                            command.as_str(),
                            desc.as_str(),
                            self.input.len(),
                        ))
                    } else {
                        None
                    }
                }))
                .collect();
        }

        // Inline `@path` file mention completion.
        if let Some(range) = self.active_mention_range() {
            return self.enumerate_path_completions(range);
        }

        Vec::new()
    }

    /// Locate the `@mention` token the cursor is currently inside, if any.
    /// Returns the byte range `(start, end)` of the token inclusive of the
    /// leading `@`. A mention only triggers completion when:
    ///
    /// - The `@` is at the start of the input or preceded by whitespace, so it
    ///   is not confused with e.g. `user@example` in pasted prose.
    /// - The cursor sits somewhere inside the `@`-prefixed run, not after a
    ///   whitespace that terminated it.
    /// - The text between `@` and the cursor contains no whitespace.
    pub fn active_mention_range(&self) -> Option<(usize, usize)> {
        mention_range_at(&self.input, self.byte_cursor())
    }

    /// Enumerate filesystem entries that extend the `@path` prefix the cursor
    /// is currently in. `mention_range` is the inclusive `(@..cursor)` byte
    /// range produced by [`Self::active_mention_range`]. Pulls from the cached
    /// recursive project scan (populated on first use) and filters with
    /// [`path_query_match`], so each keystroke only filters — it never touches
    /// the filesystem. Empty descriptions match opencode's minimal aesthetic;
    /// directories are distinguished by their trailing `/` label.
    fn enumerate_path_completions(&mut self, mention_range: (usize, usize)) -> Vec<Completion> {
        let (at_start, cursor_end) = mention_range;
        // Skip the `@` itself — only the path portion is replaced/extended.
        // Clone into an owned String so the borrow on `self.input` ends before
        // we mutably borrow `self` for the cache populate below.
        let after_at = self.input[at_start + 1..cursor_end].to_string();

        // Lazy-populate the cache on first `@` mention; subsequent calls reuse
        // it. `path_scan()` is `&mut self`, so clone the entries out to avoid
        // holding a borrow across the iterator below.
        let entries: Vec<String> = self.path_scan().entries.clone();

        let mut comps: Vec<Completion> = entries
            .iter()
            .filter(|p| path_query_match(p, &after_at))
            .take(MAX_PATH_COMPLETIONS)
            .map(|p| Completion {
                label: p.clone(),
                description: String::new(),
                replace_start: at_start + 1,
                replace_end: cursor_end,
            })
            .collect();
        // path_query_match + scan already sort, but the take() may have
        // shuffled entries between filter phases; re-sort for stability.
        comps.sort_by(|a, b| {
            let a_dir = a.label.ends_with('/');
            let b_dir = b.label.ends_with('/');
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
        });
        comps
    }

    /// Borrow the cached recursive project listing, populating it on first
    /// access. Mirrors opencode's per-directory picker cache: one
    /// [`scan_project_files`] call per App session, then pure filtering.
    fn path_scan(&mut self) -> &PathScan {
        if self.path_scan_cache.is_none() {
            self.path_scan_cache = Some(scan_project_files(&self.cwd));
        }
        self.path_scan_cache.as_ref().unwrap()
    }
}
