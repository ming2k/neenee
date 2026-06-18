//! Formatting skills for the system prompt and resolving user mentions.

use super::metadata::Skill;

/// Build a compact skills catalog for the system prompt.
pub fn build_skills_index(skills: &[Skill]) -> String {
    let enabled: Vec<&Skill> = skills.iter().filter(|s| s.enabled).collect();
    if enabled.is_empty() {
        return "No skills discovered.".to_string();
    }

    let mut lines = vec![
        "Available skills (call use_skill to load full content, or mention a skill by name):"
            .to_string(),
    ];
    for skill in enabled {
        let scope = format!("[{}]", skill.scope);
        let desc = skill
            .description
            .as_str()
            .trim()
            .is_empty()
            .then_some(
                skill
                    .short_description
                    .as_deref()
                    .unwrap_or("No description"),
            )
            .unwrap_or(skill.description.as_str());
        lines.push(format!("  - {} {}: {}", scope, skill.name, desc));
    }
    lines.join("\n")
}

/// Build a verbose listing similar to what list_skills returns.
pub fn format_skill_list(skills: &[Skill]) -> String {
    let mut lines = vec!["Available skills:".to_string()];
    for skill in skills {
        let state = if skill.enabled { "" } else { " (disabled)" };
        lines.push(format!(
            "- [{}] {}{}\n  {}",
            skill.scope,
            skill.name,
            state,
            skill
                .description
                .as_str()
                .trim()
                .is_empty()
                .then_some("No description")
                .unwrap_or(skill.description.as_str())
        ));
    }
    lines.join("\n")
}

/// Resolve which skills a piece of text is referring to.
///
/// Matches:
/// - `@skill-name`
/// - `skill://skill-name` or `skill://path/to/SKILL.md`
/// - the plain skill name as a standalone token
pub fn resolve_mentions<'a>(text: &str, skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut matched = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for skill in skills
        .iter()
        .filter(|s| s.enabled && s.allows_implicit_invocation())
    {
        if is_mentioned(text, &skill.name, &skill.source) && seen.insert(skill.name.clone()) {
            matched.push(skill);
        }
    }

    matched
}

fn is_mentioned(text: &str, name: &str, source: &std::path::Path) -> bool {
    // Direct @mention.
    if text.contains(&format!("@{}", name)) {
        return true;
    }

    // skill:// URI by name or by source path.
    let skill_uri = format!("skill://{}", name);
    if text.contains(&skill_uri) {
        return true;
    }
    let source_str = source.to_string_lossy();
    if text.contains(&format!("skill://{}", source_str)) {
        return true;
    }

    // Plain name as a standalone token. We consider tokens to be separated by
    // whitespace or common punctuation.
    for token in tokenize(text) {
        if token == name {
            return true;
        }
    }

    false
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | '.'
                    | ';'
                    | ':'
                    | '!'
                    | '?'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '"'
                    | '\''
                    | '`'
            )
    })
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_skill(name: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: "desc".to_string(),
            short_description: None,
            scope: crate::skills::SkillScope::Repo,
            source: PathBuf::from(format!("skills/{}/SKILL.md", name)),
            root: PathBuf::from(format!("skills/{}", name)),
            content: "body".to_string(),
            policy: super::super::metadata::SkillPolicy::default(),
            dependencies: vec![],
            tags: vec![],
            version: None,
            enabled: true,
        }
    }

    #[test]
    fn resolves_at_mention() {
        let skills = vec![sample_skill("rust-expert")];
        let mentions = resolve_mentions("ask @rust-expert for help", &skills);
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].name, "rust-expert");
    }

    #[test]
    fn resolves_plain_name_token() {
        let skills = vec![sample_skill("rust-expert")];
        let mentions = resolve_mentions("rust-expert: review this", &skills);
        assert_eq!(mentions.len(), 1);
    }

    #[test]
    fn does_not_match_substring() {
        let skills = vec![sample_skill("rust")];
        let mentions = resolve_mentions("rust-expert: review this", &skills);
        assert!(mentions.is_empty());
    }

    #[test]
    fn resolves_skill_uri() {
        let skills = vec![sample_skill("rust-expert")];
        let mentions = resolve_mentions("load skill://rust-expert", &skills);
        assert_eq!(mentions.len(), 1);
    }
}
