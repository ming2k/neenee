//! Project scaffolding and neenee configuration initialization.
//!
//! `CreateProjectTool` lets the agent scaffold a brand-new code project on disk
//! (Rust, Node, Python, Go, or a generic layout) with a sensible starter
//! structure, `.gitignore`, and an optional git repository.
//!
//! `init_neenee_config` materializes a `.neenee/` configuration tree in a
//! directory (skills, commands, agents) and is reused by both the
//! `init_config` tool and the `/init` slash command.

use crate::{Tool, ToolAccess};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

const MAX_PROJECT_NAME: usize = 64;

/// Create and scaffold a new project directory.
pub struct CreateProjectTool;

#[async_trait]
impl Tool for CreateProjectTool {
    fn name(&self) -> &str {
        "create_project"
    }
    fn description(&self) -> &str {
        "Scaffold a brand-new code project on disk. Generates a sensible starter structure for \
         the chosen language (rust, node, python, go, or generic), including entrypoint files, \
         manifest, .gitignore, and README. Optionally initializes a git repository. Use this when \
         the user asks to create/start a new project, app, package, or workspace."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Project (and directory) name. Must be a valid folder name." },
                "type": {
                    "type": "string",
                    "enum": ["rust", "node", "python", "go", "generic"],
                    "description": "Project template to use"
                },
                "path": { "type": "string", "description": "Parent directory to create the project in (default '.')" },
                "git": { "type": "boolean", "description": "Run `git init` in the new project (default true)" },
                "neenee": { "type": "boolean", "description": "Also initialize a .neenee/ config tree in the project (default false)" }
            },
            "required": ["name", "type"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }
    fn permission_scope(&self, arguments: &str) -> String {
        let name = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("name")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "*".to_string());
        let path = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("path")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ".".to_string());
        format!("{}/{}", path, name)
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let name = args["name"].as_str().ok_or("Missing 'name'")?;
        let project_type = args["type"].as_str().ok_or("Missing 'type'")?;
        let parent = args["path"].as_str().unwrap_or(".");
        let want_git = args["git"].as_bool().unwrap_or(true);
        let want_neenee = args["neenee"].as_bool().unwrap_or(false);

        validate_project_name(name)?;
        let project_dir = PathBuf::from(parent).join(name);
        if project_dir.exists() {
            return Err(format!(
                "A directory named '{}' already exists at '{}'. Choose a different name or path.",
                name, parent
            ));
        }

        let files = scaffold(project_type, name)?;
        write_files(&project_dir, &files)?;

        if want_git {
            init_git(&project_dir)?;
        }

        let mut created: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
        if want_neenee {
            for path in init_neenee_config(&project_dir)? {
                created.push(path);
            }
        }

        created.sort();
        Ok(format!(
            "Created '{}' ({}) project at {}.\nFiles:\n{}",
            name,
            project_type,
            project_dir.display(),
            created
                .iter()
                .map(|path| format!("- {}", path))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

/// Initialize a `.neenee/` configuration tree in a new or existing project.
pub struct InitConfigTool;

#[async_trait]
impl Tool for InitConfigTool {
    fn name(&self) -> &str {
        "init_config"
    }
    fn description(&self) -> &str {
        "Initialize a neenee configuration tree (`.neenee/` with skills, commands, and agents \
         directories, plus an AGENTS.md guide) in the given directory. Idempotent: existing files \
         are never overwritten. Use when the user wants to set up neenee for a project."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to initialize (default current dir)" }
            },
            "required": []
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }
    fn permission_scope(&self, arguments: &str) -> String {
        serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("path")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ".".to_string())
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let base = args["path"].as_str().unwrap_or(".");
        let base_path = PathBuf::from(base);
        std::fs::create_dir_all(&base_path)
            .map_err(|e| format!("Failed to access '{}': {}", base, e))?;
        let created = init_neenee_config(&base_path)?;
        if created.is_empty() {
            return Ok(format!(
                "neenee is already configured in '{}'. Nothing to do.",
                base
            ));
        }
        Ok(format!(
            "Initialized neenee configuration in '{}'.\nCreated:\n{}",
            base,
            created
                .iter()
                .map(|path| format!("- {}", path))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn validate_project_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Project name must not be empty.".to_string());
    }
    if name.len() > MAX_PROJECT_NAME {
        return Err(format!(
            "Project name is too long (max {} characters).",
            MAX_PROJECT_NAME
        ));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err("Project name must not start with '-' or '.'.".to_string());
    }
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != '.' {
            return Err(format!(
                "Project name '{}' contains an invalid character '{}'. Use letters, digits, '-', '_', or '.'.",
                name, ch
            ));
        }
    }
    if name == "." || name == ".." {
        return Err("Project name must not be '.' or '..'.".to_string());
    }
    Ok(())
}

/// A project template is a list of (relative path, file content) pairs.
type Template = Vec<(String, String)>;

fn scaffold(project_type: &str, name: &str) -> Result<Template, String> {
    match project_type {
        "rust" => Ok(rust_template(name)),
        "node" => Ok(node_template(name)),
        "python" => Ok(python_template(name)),
        "go" => Ok(go_template(name)),
        "generic" => Ok(generic_template(name)),
        other => Err(format!(
            "Unknown project type '{}'. Use rust, node, python, go, or generic.",
            other
        )),
    }
}

fn rust_template(name: &str) -> Template {
    let crate_name = name.replace('-', "_");
    vec![
        (
            "Cargo.toml".to_string(),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
                crate_name
            ),
        ),
        (
            "src/main.rs".to_string(),
            "fn main() {\n    println!(\"Hello, world!\");\n}\n".to_string(),
        ),
        ("README.md".to_string(), format!("# {}\n\nA Rust project.\n", name)),
        (".gitignore".to_string(), rust_gitignore().to_string()),
    ]
}

fn node_template(name: &str) -> Template {
    vec![
        (
            "package.json".to_string(),
            format!(
                "{{\n  \"name\": \"{}\",\n  \"version\": \"0.1.0\",\n  \"private\": true,\n  \"type\": \"module\",\n  \"main\": \"index.js\",\n  \"scripts\": {{\n    \"start\": \"node index.js\"\n  }}\n}}\n",
                name
            ),
        ),
        (
            "index.js".to_string(),
            "console.log(\"Hello, world!\");\n".to_string(),
        ),
        ("README.md".to_string(), format!("# {}\n\nA Node.js project.\n", name)),
        (".gitignore".to_string(), node_gitignore().to_string()),
    ]
}

fn python_template(name: &str) -> Template {
    let module = name.replace('-', "_");
    vec![
        (
            "main.py".to_string(),
            "#!/usr/bin/env python3\n\n\ndef main() -> None:\n    print(\"Hello, world!\")\n\n\nif __name__ == \"__main__\":\n    main()\n".to_string(),
        ),
        ("requirements.txt".to_string(), String::new()),
        (
            "README.md".to_string(),
            format!("# {}\n\nA Python project.\n\n```\npython main.py\n```\n", name),
        ),
        (".gitignore".to_string(), python_gitignore().to_string()),
        (format!("{}/__init__.py", module), String::new()),
    ]
}

fn go_template(name: &str) -> Template {
    let module = format!("example.com/{}", name);
    vec![
        ("go.mod".to_string(), format!("module {}\n\ngo 1.21\n", module)),
        (
            "main.go".to_string(),
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"Hello, world!\")\n}\n".to_string(),
        ),
        ("README.md".to_string(), format!("# {}\n\nA Go project.\n", name)),
        (".gitignore".to_string(), go_gitignore().to_string()),
    ]
}

fn generic_template(name: &str) -> Template {
    vec![
        (
            "README.md".to_string(),
            format!("# {}\n\nA new project.\n", name),
        ),
        (".gitignore".to_string(), generic_gitignore().to_string()),
    ]
}

fn write_files(project_dir: &Path, files: &[(String, String)]) -> Result<(), String> {
    for (relative, content) in files {
        let path = project_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {}", parent.display(), e))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write '{}': {}", path.display(), e))?;
    }
    Ok(())
}

fn init_git(project_dir: &Path) -> Result<(), String> {
    let result = std::process::Command::new("git")
        .arg("init")
        .current_dir(project_dir)
        .output();
    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("git init failed: {}", stderr.trim()))
        }
        Err(_) => Ok(()),
    }
}

/// Materialize a `.neenee/` tree. Returns the list of newly created relative
/// paths (existing files are left untouched and not reported).
pub fn init_neenee_config(base: &Path) -> Result<Vec<String>, String> {
    let mut created = Vec::new();
    let dirs = ["skills", "commands", "agents"];
    for dir in dirs {
        let path = base.join(".neenee").join(dir);
        if !path.exists() {
            std::fs::create_dir_all(&path)
                .map_err(|e| format!("Failed to create '{}': {}", path.display(), e))?;
            created.push(format!(".neenee/{}/.keep", dir));
            std::fs::write(path.join(".keep"), "")
                .map_err(|e| format!("Failed to write keep file: {}", e))?;
        }
    }

    let agents_md = base.join("AGENTS.md");
    if !agents_md.exists() {
        std::fs::write(&agents_md, agents_md_template(base))
            .map_err(|e| format!("Failed to write AGENTS.md: {}", e))?;
        created.push("AGENTS.md".to_string());
    }

    let gitignore = base.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, neenee_gitignore())
            .map_err(|e| format!("Failed to write .gitignore: {}", e))?;
        created.push(".gitignore".to_string());
    }

    Ok(created)
}

fn agents_md_template(base: &Path) -> String {
    let project_name = base
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("this project");
    format!(
        "# {name} — Agent Guide\n\n\
         Background, architecture, and conventions coding agents need to work\n\
         effectively in this repository. Fill in the sections below as the\n\
         project matures.\n\n\
         ## Overview\n\n\
         Describe what `{name}` does and its high-level architecture.\n\n\
         ## Build & Test\n\n\
         ```\n\
         # build\n\
         # test\n\
         # lint\n\
         ```\n\n\
         ## Conventions\n\n\
         - Coding style and patterns\n\
         - Where new code should go\n\
         - Anything an agent must know before editing\n",
        name = project_name
    )
}

fn neenee_gitignore() -> &'static str {
    "# neenee\n.neenee/session.json\n.neenee/sessions/\n"
}

fn rust_gitignore() -> &'static str {
    "/target\n**/*.rs.bk\nCargo.lock.bak\n"
}

fn node_gitignore() -> &'static str {
    "node_modules/\nnpm-debug.log*\nyarn-debug.log*\nyarn-error.log*\n.env\n.DS_Store\n"
}

fn python_gitignore() -> &'static str {
    "__pycache__/\n*.py[cod]\n*.egg-info/\n.venv/\nvenv/\n.env\n.DS_Store\n"
}

fn go_gitignore() -> &'static str {
    "*.exe\n*.exe~\n*.dll\n*.so\n*.dylib\n*.test\n*.out\nvendor/\n"
}

fn generic_gitignore() -> &'static str {
    ".DS_Store\n.env\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_validation_rejects_invalid_input() {
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("-nope").is_err());
        assert!(validate_project_name("../escape").is_err());
        assert!(validate_project_name("has space").is_err());
        assert!(validate_project_name("ok_name").is_ok());
        assert!(validate_project_name("my-cool.app").is_ok());
    }

    #[test]
    fn rust_template_uses_valid_crate_name() {
        let template = rust_template("my-cool-app");
        let cargo = template
            .iter()
            .find(|(path, _)| *path == "Cargo.toml")
            .unwrap();
        assert!(cargo.1.contains("name = \"my_cool_app\""));
        assert!(template.iter().any(|(path, _)| *path == "src/main.rs"));
    }

    #[test]
    fn go_template_includes_module() {
        let template = go_template("widget");
        let modfile = template.iter().find(|(p, _)| *p == "go.mod").unwrap();
        assert!(modfile.1.contains("module example.com/widget"));
    }

    #[test]
    fn init_neenee_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("neenee-init-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = init_neenee_config(&dir).unwrap();
        assert!(first.iter().any(|p| p == "AGENTS.md"));
        let second = init_neenee_config(&dir).unwrap();
        assert!(second.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_project_scaffolds_rust_project_on_disk() {
        let dir = std::env::temp_dir().join(format!("neenee-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = CreateProjectTool;
        let arguments = format!(
            "{{\"name\":\"demo-app\",\"type\":\"rust\",\"path\":\"{}\",\"git\":false}}",
            dir.display()
        );
        let output = tool.call(&arguments).await.unwrap();
        assert!(output.contains("demo-app"));
        let project = dir.join("demo-app");
        assert!(project.join("Cargo.toml").exists());
        assert!(project.join("src/main.rs").exists());
        assert!(project.join(".gitignore").exists());
        let cargo = std::fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"demo_app\""));
        // No git repo requested
        assert!(!project.join(".git").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_project_refuses_existing_directory() {
        let dir = std::env::temp_dir().join(format!("neenee-dup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("taken")).unwrap();
        let tool = CreateProjectTool;
        let arguments = format!(
            "{{\"name\":\"taken\",\"type\":\"generic\",\"path\":\"{}\",\"git\":false}}",
            dir.display()
        );
        assert!(tool.call(&arguments).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
