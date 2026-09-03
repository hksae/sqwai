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
            std::env::var_os("SQWAI_SHELL")
                .or_else(|| std::env::var_os("SHELL"))
                .or_else(|| std::env::var_os("ComSpec"))
                .or_else(|| std::env::var_os("COMSPEC"))
                .and_then(|path| Self::from_program(Path::new(&path)))
                .unwrap_or(Self::Cmd)
        }
        #[cfg(not(windows))]
        {
            std::env::var_os("SQWAI_SHELL")
                .or_else(|| std::env::var_os("SHELL"))
                .and_then(|path| Self::from_program(Path::new(&path)))
                .unwrap_or(Self::Sh)
        }
    }

    fn from_program(path: &Path) -> Option<Self> {
        let name = path.file_stem()?.to_str()?.to_ascii_lowercase();
        match name.as_str() {
            "bash" => Some(Self::Bash),
            "sh" | "dash" | "zsh" | "fish" | "ksh" => Some(Self::Sh),
            "cmd" => Some(Self::Cmd),
            "pwsh" | "powershell" => Some(Self::PowerShell),
            _ => None,
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
