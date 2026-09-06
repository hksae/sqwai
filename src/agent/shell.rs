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

    /// Recognize the shell from a program path, under either separator.
    ///
    /// `Path::file_stem` is platform-dependent: on Unix `\` is an ordinary
    /// character, so a Windows-style `SHELL` value such as
    /// `C:\Windows\System32\cmd.exe` is one single component and `file_stem`
    /// returns the whole string. That made every such value fall through to
    /// the `Sh` default — and shell detection decides which layer of the
    /// dangerous-command classifier is authoritative (§5.2).
    fn from_program(path: &Path) -> Option<Self> {
        let segment = path.to_str()?.rsplit(['/', '\\']).next()?;
        let stem = segment.rsplit_once('.').map_or(segment, |(stem, _)| stem);
        match stem.to_ascii_lowercase().as_str() {
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
            Self::PowerShell => {
                #[cfg(windows)]
                {
                    ("powershell", "-Command")
                }
                #[cfg(not(windows))]
                {
                    ("pwsh", "-Command")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_names_are_normalized_from_paths() {
        assert_eq!(
            ShellKind::from_program(Path::new("C:\\Windows\\System32\\cmd.exe")),
            Some(ShellKind::Cmd)
        );
        assert_eq!(
            ShellKind::from_program(Path::new("C:\\Program Files\\PowerShell\\pwsh.exe")),
            Some(ShellKind::PowerShell)
        );
        assert_eq!(
            ShellKind::from_program(Path::new("/usr/bin/bash")),
            Some(ShellKind::Bash)
        );
        assert_eq!(ShellKind::from_program(Path::new("unknown-shell")), None);
    }

    #[test]
    fn shell_command_invocation_has_expected_flag() {
        assert_eq!(ShellKind::Bash.program_and_flag().1, "-c");
        assert_eq!(ShellKind::Sh.program_and_flag().1, "-c");
        assert_eq!(ShellKind::Cmd.program_and_flag().1, "/C");
        assert_eq!(ShellKind::PowerShell.program_and_flag().1, "-Command");
    }
}
