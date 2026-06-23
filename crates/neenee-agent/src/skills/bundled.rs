//! Compile-time-embedded system skills.
//!
//! Bundled skills live under `crates/neenee-agent/skills/bundled/<name>/`
//! and are baked into the binary via `include_dir!`. They surface as
//! [`SkillScope::System`] — the lowest-priority source in the discovery
//! cascade, so any user / project / remote skill with the same name wins.
//!
//! Rationale (see ADR-0013): the previous design shipped bundled skills at
//! `~/.neenee/skills/.system/`, which violated XDG, mixed read-only payload
//! with user-writable storage, and relied on a hidden-directory naming hack
//! to avoid double-counting. Compile-time embedding removes all three issues
//! and needs no install or sync step.

use std::path::PathBuf;

use include_dir::{Dir, DirEntry};

use super::metadata::{parse_skill_from_str, Skill, SkillScope};

/// The embedded tree, rooted at `crates/neenee-agent/skills/bundled/`.
static BUNDLED: Dir<'static> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/skills/bundled");

/// Parse every `SKILL.md` in the embedded tree into a [`Skill`] with
/// [`SkillScope::System`]. Errors are surfaced per-file (via `tracing::warn`)
/// rather than fatal: a single malformed bundled skill should not prevent the
/// rest from loading.
///
/// Embedded paths are synthesised as `bundled/<name>/SKILL.md` so that
/// `Skill::source` / `Skill::root` stay informative in diagnostics even
/// though no real filesystem location exists.
pub fn discover() -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    collect(&BUNDLED, PathBuf::from("bundled"), &mut skills, &mut errors);
    for err in &errors {
        tracing::warn!("bundled skill parse error: {}", err);
    }
    skills
}

fn collect(dir: &Dir<'_>, prefix: PathBuf, skills: &mut Vec<Skill>, errors: &mut Vec<String>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(child) => {
                let child_prefix = prefix.join(child.path());
                collect(child, child_prefix, skills, errors);
            }
            DirEntry::File(file) => {
                let is_skill_md = file
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == "SKILL.md")
                    .unwrap_or(false);
                if !is_skill_md {
                    continue;
                }

                let source = prefix.join(file.path());
                let root = source.parent().unwrap_or(&prefix).to_path_buf();
                let body = match std::str::from_utf8(file.contents()) {
                    Ok(s) => s,
                    Err(e) => {
                        errors.push(format!("'{}' is not valid UTF-8: {}", source.display(), e));
                        continue;
                    }
                };
                match parse_skill_from_str(&source, &root, SkillScope::System, true, body) {
                    Ok(skill) => skills.push(skill),
                    Err(e) => errors.push(format!("'{}': {}", source.display(), e)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_returns_empty_without_crashing() {
        // No bundled skills ship yet. The embedded tree contains only
        // README.md, so `discover` should find zero SKILL.md files.
        let skills = discover();
        assert!(
            skills.is_empty(),
            "expected no bundled skills, got {:?}",
            skills
        );
    }
}
