use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{config::AppConfig, types::Provider};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn display(&self) -> String {
        std::iter::once(shell_quote(&self.program))
            .chain(self.args.iter().map(|arg| shell_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn build_resume_command(
    config: &AppConfig,
    provider: Provider,
    profile: &str,
    session_id: &str,
    prompt: Option<&str>,
    cwd: Option<PathBuf>,
) -> Result<CommandSpec> {
    let provider_config = config.provider(provider);
    let profile_args = provider_config
        .profiles
        .get(profile)
        .or_else(|| provider_config.profiles.get("default"))
        .with_context(|| format!("profile `{profile}` is not defined for {provider}"))?;

    let mut args = Vec::new();
    for template in &provider_config.resume_args {
        match template.as_str() {
            "{profile_args}" => args.extend(profile_args.iter().cloned()),
            "{session_id}" => args.push(session_id.to_string()),
            "{prompt}" => {
                if let Some(prompt) = prompt.filter(|prompt| !prompt.is_empty()) {
                    args.push(prompt.to_string());
                }
            }
            arg if arg.contains('{') => {
                let expanded = arg
                    .replace("{session_id}", session_id)
                    .replace("{prompt}", prompt.unwrap_or(""))
                    .replace(
                        "{cwd}",
                        cwd.as_ref()
                            .map(|cwd| cwd.display().to_string())
                            .as_deref()
                            .unwrap_or(""),
                    );
                if expanded.contains("{profile_args}") {
                    bail!("{{profile_args}} must be a standalone resume arg template");
                }
                if !expanded.is_empty() {
                    args.push(expanded);
                }
            }
            arg => args.push(arg.to_string()),
        }
    }

    Ok(CommandSpec {
        program: provider_config.command.clone(),
        args,
        cwd,
    })
}

pub fn run_command(spec: &CommandSpec) -> Result<i32> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    let status = command
        .status()
        .with_context(|| format!("failed to run `{}`", spec.program))?;
    Ok(status.code().unwrap_or(1))
}

fn shell_quote(input: &str) -> String {
    if input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        input.to_string()
    } else {
        format!("'{}'", input.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_codex_yolo_args_without_shell_joining() {
        let config = AppConfig::default();
        let spec = build_resume_command(
            &config,
            Provider::Codex,
            "yolo",
            "session-1",
            Some("hello; rm -rf /"),
            Some(PathBuf::from("/tmp/project")),
        )
        .unwrap();
        assert_eq!(spec.program, "codex");
        assert_eq!(spec.args[0], "resume");
        assert!(spec.args.contains(&"--yolo".to_string()));
        assert!(spec.args.contains(&"hello; rm -rf /".to_string()));
    }

    #[test]
    fn renders_claude_resume() {
        let config = AppConfig::default();
        let spec =
            build_resume_command(&config, Provider::Claude, "yolo", "abc", None, None).unwrap();
        assert_eq!(spec.program, "claude");
        assert_eq!(
            spec.args,
            vec!["--dangerously-skip-permissions", "--resume", "abc"]
        );
    }
}
