//! Shell identity shared by command execution and safety classification.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Sh,
    Cmd,
    PowerShell,
}

impl ShellKind {
    pub fn detect() -> Self {
        #[cfg(windows)]
        {
            let shell = std::env::var_os("SHELL")
                .or_else(|| std::env::var_os("ComSpec"))
                .or_else(|| std::env::var_os("COMSPEC"));
            if let Some(path) = shell {
                let name = Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if name == "bash" || name == "sh" || name == "zsh" {
                    return if name == "bash" { Self::Bash } else { Self::Sh };
                }
                if name == "pwsh" || name == "powershell" {
                    return Self::PowerShell;
                }
            }
            if std::env::var_os("PSModulePath").is_some() {
                return Self::PowerShell;
            }
            Self::Cmd
        }
        #[cfg(not(windows))]
        {
            let name = std::env::var_os("SHELL")
                .and_then(|p| Path::new(&p).file_stem().map(|s| s.to_owned()))
                .and_then(|s| s.to_str().map(str::to_ascii_lowercase));
            match name.as_deref() {
                Some("bash") => Self::Bash,
                _ => Self::Sh,
            }
        }
    }

    pub fn program_and_flag(self) -> (&'static str, &'static str) {
        match self {
            Self::Bash => ("bash", "-c"),
            Self::Sh => ("/bin/sh", "-c"),
            Self::Cmd => ("cmd", "/C"),
            Self::PowerShell => ("powershell", "-Command"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_invocation_has_expected_flag() {
        assert_eq!(ShellKind::Bash.program_and_flag().1, "-c");
        assert_eq!(ShellKind::Sh.program_and_flag().1, "-c");
        assert_eq!(ShellKind::Cmd.program_and_flag().1, "/C");
        assert_eq!(ShellKind::PowerShell.program_and_flag().1, "-Command");
    }
}
