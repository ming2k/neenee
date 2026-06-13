//! Project and user-defined slash command templates.
//!
//! Commands are markdown files stored in:
//!   - Project-local: `.neenee/commands/` (highest priority)
//!   - User-global: `~/.neenee/commands/` (fallback)

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: Option<String>,
    pub source: PathBuf,
    pub template: String,
}

#[derive(Debug, Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

pub fn discover_commands() -> Vec<CustomCommand> {
    discover_commands_in(&[project_commands_dir(), user_commands_dir()])
}

fn discover_commands_in(dirs: &[PathBuf]) -> Vec<CustomCommand> {
    let mut commands = Vec::new();
    let mut seen_names = HashSet::new();

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("md"))
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            let Some(command) = parse_command_file(&path) else {
                continue;
            };
            if seen_names.insert(command.name.clone()) {
                commands.push(command);
            }
        }
    }

    commands
}

fn parse_command_file(path: &Path) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&raw)?;
    let meta: Frontmatter = serde_yaml::from_str(frontmatter).unwrap_or_default();
    let name = meta.name.unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let name = name.trim().trim_start_matches('/').to_ascii_lowercase();
    if !valid_command_name(&name) || body.trim().is_empty() {
        return None;
    }

    Some(CustomCommand {
        name,
        description: meta.description,
        source: path.to_path_buf(),
        template: body.trim().to_string(),
    })
}

pub fn expand_command(command: &CustomCommand, arguments: &str) -> String {
    let positional = split_arguments(arguments);
    let mut expanded = command.template.replace("$ARGUMENTS", arguments.trim());
    for index in (1..=9).rev() {
        expanded = expanded.replace(
            &format!("${index}"),
            positional.get(index - 1).map(String::as_str).unwrap_or(""),
        );
    }
    expanded
}

fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return Some(("", trimmed));
    }
    let after_open = &trimmed[3..];
    let close_idx = after_open.find("---")?;
    Some((after_open[..close_idx].trim(), &after_open[close_idx + 3..]))
}

fn split_arguments(input: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for value in input.chars() {
        if escaped {
            current.push(value);
            escaped = false;
            continue;
        }
        if value == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(value, '\'' | '"') {
            if quote == Some(value) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(value);
            } else {
                current.push(value);
            }
            continue;
        }
        if value.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(value);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

fn user_commands_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".neenee")
        .join("commands")
}

fn project_commands_dir() -> PathBuf {
    PathBuf::from(".neenee/commands")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_raw_and_positional_arguments() {
        let command = CustomCommand {
            name: "review".to_string(),
            description: None,
            source: PathBuf::from("review.md"),
            template: "Review $1 against $2. Full: $ARGUMENTS".to_string(),
        };

        assert_eq!(
            expand_command(&command, "\"working tree\" main"),
            "Review working tree against main. Full: \"working tree\" main"
        );
    }

    #[test]
    fn parses_frontmatter_and_rejects_invalid_names() {
        let root = std::env::temp_dir().join(format!("neenee-command-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let user = root.join("user");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            project.join("review.md"),
            "---\ndescription: Review changes\n---\nInspect $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(user.join("review.md"), "lower priority").unwrap();
        std::fs::write(project.join("bad name.md"), "ignored").unwrap();

        let commands = discover_commands_in(&[project, user]);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description.as_deref(), Some("Review changes"));
        assert_eq!(commands[0].template, "Inspect $ARGUMENTS");

        std::fs::remove_dir_all(root).unwrap();
    }
}
