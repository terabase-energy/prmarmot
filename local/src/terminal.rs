//! Open a PR checkout in a new terminal tab that runs an agent CLI with a
//! skill. The agent runs on the person's CLI login, not on an API key.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The agent command when the setting is blank.
pub const DEFAULT_AGENT: &str = "claude";

/// The first macOS launch waits for the Automation consent dialog, so the
/// limit is long.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Wrap `value` in single quotes for a POSIX shell.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The skill name without a leading `/` or `$`, or `None` when it is blank.
/// Returns an error when the name has characters that a skill name cannot
/// have.
pub fn skill_name(skill: &str) -> Result<Option<&str>, String> {
    let name = skill.trim().trim_start_matches(['/', '$']);
    if name.is_empty() {
        return Ok(None);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(format!(
            "Terminal skill {name:?} is not valid; use letters, digits, - _ . :"
        ));
    }
    Ok(Some(name))
}

/// The shell command that starts `agent` with `skill` on `pr_url`. With no
/// skill, the agent starts with no prompt and waits for input.
///
/// Codex mentions a skill with `$name`; Claude Code and other agents use
/// `/name`. For Claude Code, the command removes `ANTHROPIC_API_KEY` and
/// `ANTHROPIC_AUTH_TOKEN`, because they come before the subscription login.
/// The session is interactive (no `-p`), so the person can follow it.
pub fn agent_command(agent: &str, skill: &str, pr_url: &str) -> Result<String, String> {
    let skill = skill_name(skill)?;
    let agent = match agent.trim() {
        "" => DEFAULT_AGENT,
        agent => agent,
    };
    let program = agent
        .split_whitespace()
        .next()
        .and_then(|word| word.rsplit('/').next())
        .unwrap_or(agent);
    let prefix = if program == "codex" { '$' } else { '/' };
    let env = if program == "claude" {
        "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN "
    } else {
        ""
    };
    Ok(match skill {
        Some(skill) => {
            let prompt = format!("{prefix}{skill} {pr_url}");
            format!("{env}{agent} {}", shell_quote(&prompt))
        }
        None => format!("{env}{agent}"),
    })
}

/// The full line the new tab runs: go to `dir`, then start the agent.
pub fn tab_command(dir: &Path, agent_command: &str) -> Result<String, String> {
    let dir = dir.to_str().ok_or("Checkout path is not UTF-8")?;
    Ok(format!("cd {} && {agent_command}", shell_quote(dir)))
}

/// A terminal that PR Marmot can open a tab in.
#[derive(Debug, PartialEq, Eq)]
pub enum Terminal {
    /// iTerm2, with AppleScript: a new tab in the front window.
    Iterm,
    /// macOS Terminal, with AppleScript: a new window, because a new tab
    /// needs Accessibility access.
    TerminalApp,
    /// A shell command template. `{command}` and `{dir}` are replaced with
    /// shell-quoted values.
    Custom(String),
}

impl Terminal {
    /// Parse the setting. Blank means macOS Terminal.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.contains("{command}") {
            return Ok(Self::Custom(value.to_owned()));
        }
        match value.to_ascii_lowercase().as_str() {
            "iterm" | "iterm2" | "iterm.app" => Ok(Self::Iterm),
            "" | "terminal" | "terminal.app" => Ok(Self::TerminalApp),
            _ => Err(format!(
                "Terminal {value:?} is not supported; use iTerm, Terminal, or a command with {{command}}"
            )),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Iterm => "iTerm",
            Self::TerminalApp => "Terminal",
            Self::Custom(_) => "the terminal",
        }
    }
}

/// Fill a custom template.
pub fn fill_template(template: &str, dir: &Path, command: &str) -> Result<String, String> {
    let dir = dir.to_str().ok_or("Checkout path is not UTF-8")?;
    Ok(template
        .replace("{dir}", &shell_quote(dir))
        .replace("{command}", &shell_quote(command)))
}

const ITERM_SCRIPT: &str = r#"on run argv
set cmd to item 1 of argv
set wasRunning to application "iTerm" is running
tell application "iTerm"
activate
if not wasRunning then
repeat 30 times
if (count of windows) > 0 then exit repeat
delay 0.1
end repeat
end if
if (count of windows) = 0 then
create window with default profile
else if wasRunning then
tell current window to create tab with default profile
end if
tell current session of current window to write text cmd
end tell
end run"#;

const TERMINAL_SCRIPT: &str = r#"on run argv
set cmd to item 1 of argv
set wasRunning to application "Terminal" is running
tell application "Terminal"
activate
if not wasRunning then
repeat 30 times
if (count of windows) > 0 then exit repeat
delay 0.1
end repeat
end if
if (not wasRunning) and (count of windows) > 0 then
do script cmd in window 1
else
do script cmd
end if
end tell
end run"#;

/// Open a new tab (or window) in `terminal` that runs `command` in `dir`.
pub fn open(terminal: &Terminal, dir: &Path, command: &str) -> Result<(), String> {
    let line = tab_command(dir, command)?;
    let mut cmd = match terminal {
        Terminal::Custom(template) => {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(fill_template(template, dir, &line)?);
            cmd
        }
        Terminal::Iterm | Terminal::TerminalApp => {
            if !cfg!(target_os = "macos") {
                return Err(format!(
                    "{} is macOS only; set Terminal to a command with {{command}}",
                    terminal.name()
                ));
            }
            let script = if *terminal == Terminal::Iterm {
                ITERM_SCRIPT
            } else {
                TERMINAL_SCRIPT
            };
            let mut cmd = Command::new("/usr/bin/osascript");
            for statement in script.lines() {
                cmd.arg("-e").arg(statement);
            }
            cmd.arg(line);
            cmd
        }
    };
    let output = prmarmot_core::process::run_bounded(&mut cmd, LAUNCH_TIMEOUT, 16 * 1024, false)
        .map_err(|e| format!("Could not start {}: {e}", terminal.name()))?
        .ok_or_else(|| format!("{} did not answer in time", terminal.name()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // -1743: the person did not allow PR Marmot to control the app.
    if stderr.contains("-1743") {
        return Err(format!(
            "Allow PR Marmot to control {} in System Settings → Privacy & Security → Automation",
            terminal.name()
        ));
    }
    Err(format!(
        "Could not open {}: {}",
        terminal.name(),
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/acme/widget/pull/7";

    #[test]
    fn claude_runs_on_the_subscription_login() {
        assert_eq!(
            agent_command("", "code-review", URL).unwrap(),
            "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN claude \
             '/code-review https://github.com/acme/widget/pull/7'"
        );
        assert_eq!(
            agent_command("/opt/bin/claude --model opus", "/review", URL).unwrap(),
            "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN /opt/bin/claude --model opus \
             '/review https://github.com/acme/widget/pull/7'"
        );
    }

    #[test]
    fn codex_mentions_the_skill_with_a_dollar() {
        assert_eq!(
            agent_command(" codex ", "$code-review", URL).unwrap(),
            "codex '$code-review https://github.com/acme/widget/pull/7'"
        );
    }

    #[test]
    fn no_skill_starts_the_agent_waiting_for_input() {
        assert_eq!(
            agent_command("", "  ", URL).unwrap(),
            "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN claude"
        );
        assert_eq!(agent_command("codex", "", URL).unwrap(), "codex");
    }

    #[test]
    fn skill_must_be_a_plain_name() {
        assert!(agent_command("claude", "x'; rm -rf ~", URL).is_err());
        assert_eq!(
            skill_name("plugin:skill.v2_a-b").unwrap(),
            Some("plugin:skill.v2_a-b")
        );
        assert_eq!(skill_name(" / ").unwrap(), None);
    }

    #[test]
    fn quoting_keeps_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            tab_command(Path::new("/tmp/a b"), "claude").unwrap(),
            "cd '/tmp/a b' && claude"
        );
    }

    #[test]
    fn terminal_setting_parses() {
        assert_eq!(Terminal::parse("").unwrap(), Terminal::TerminalApp);
        assert_eq!(Terminal::parse(" iTerm2 ").unwrap(), Terminal::Iterm);
        assert_eq!(Terminal::parse("iterm").unwrap(), Terminal::Iterm);
        assert_eq!(
            Terminal::parse("wezterm cli spawn -- zsh -lic {command}").unwrap(),
            Terminal::Custom("wezterm cli spawn -- zsh -lic {command}".into())
        );
        assert!(Terminal::parse("Ghostty").is_err());
    }

    #[test]
    fn template_gets_quoted_values() {
        assert_eq!(
            fill_template(
                "wezterm cli spawn --cwd {dir} -- zsh -lic {command}",
                Path::new("/w/x"),
                "cd '/w/x' && claude"
            )
            .unwrap(),
            r"wezterm cli spawn --cwd '/w/x' -- zsh -lic 'cd '\''/w/x'\'' && claude'"
        );
    }
}
