//! Shell identity shared by command execution and safety classification.

use std::path::{Path, PathBuf};

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
            let explicit = std::env::var_os("SQWAI_SHELL")
                .or_else(|| std::env::var_os("SHELL"))
                .map(PathBuf::from);
            let comspec = std::env::var_os("ComSpec")
                .or_else(|| std::env::var_os("COMSPEC"))
                .map(PathBuf::from);
            Self::pick(
                explicit.as_deref(),
                Self::git_bash().is_some(),
                comspec.as_deref(),
            )
        }
        #[cfg(not(windows))]
        {
            std::env::var_os("SQWAI_SHELL")
                .or_else(|| std::env::var_os("SHELL"))
                .and_then(|path| Self::from_program(Path::new(&path)))
                .unwrap_or(Self::Sh)
        }
    }

    /// Which shell wins on Windows, given the environment's hints. Pure, so the
    /// order is testable without a Windows box: an explicit choice beats an
    /// installed Git Bash, which beats the system default.
    ///
    /// `ComSpec` is not a preference — it is what Windows defaults to, and under
    /// it every `tail`, every pipeline and every `;` the model writes dies with
    /// "is not recognized as an internal or external command". Git for Windows
    /// ships a real shell; if it is installed, use it.
    #[cfg(windows)]
    fn pick(explicit: Option<&Path>, git_bash_installed: bool, comspec: Option<&Path>) -> Self {
        if let Some(kind) = explicit.and_then(Self::from_program) {
            return kind;
        }
        if git_bash_installed {
            return Self::Bash;
        }
        comspec.and_then(Self::from_program).unwrap_or(Self::Cmd)
    }

    /// Where Git for Windows keeps `bash.exe`. Resolved once and cached: shell
    /// identity is asked for on every command, both to spawn it and to classify
    /// it (§5.2), and a filesystem probe does not belong in that path.
    #[cfg(windows)]
    pub fn git_bash() -> Option<&'static Path> {
        use std::sync::OnceLock;
        static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
        FOUND
            .get_or_init(|| {
                let from_env = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
                    .iter()
                    .filter_map(std::env::var_os)
                    .map(PathBuf::from)
                    .map(|base| base.join("Git").join("bin").join("bash.exe"))
                    .find(|path| path.is_file());
                from_env.or_else(|| {
                    Path::new(r"C:\Program Files\Git\bin\bash.exe")
                        .is_file()
                        .then(|| PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"))
                })
            })
            .as_deref()
    }

    /// The program to spawn. Bare `bash` is not on PATH on a default Git for
    /// Windows install, so the resolved path is what has to go to `Command::new`.
    pub fn program_and_flag(self) -> (std::borrow::Cow<'static, str>, &'static str) {
        use std::borrow::Cow;
        match self {
            Self::Bash => (
                #[cfg(windows)]
                {
                    Self::git_bash().map_or(Cow::Borrowed("bash"), |path| {
                        Cow::Owned(path.to_string_lossy().into_owned())
                    })
                },
                #[cfg(not(windows))]
                {
                    Cow::Borrowed("bash")
                },
                "-c",
            ),
            Self::Sh => (Cow::Borrowed("/bin/sh"), "-c"),
            Self::Cmd => (Cow::Borrowed("cmd"), "/C"),
            Self::PowerShell => (
                #[cfg(windows)]
                {
                    Cow::Borrowed("powershell")
                },
                #[cfg(not(windows))]
                {
                    Cow::Borrowed("pwsh")
                },
                "-Command",
            ),
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

    /// The order the Windows default is chosen in. `ComSpec` is what the
    /// system defaults to, not what anyone picked, so an installed Git Bash
    /// outranks it — while a shell named in the environment outranks everything.
    #[cfg(windows)]
    #[test]
    fn an_explicit_shell_beats_git_bash_and_git_bash_beats_the_default() {
        let comspec = Path::new(r"C:\Windows\System32\cmd.exe");
        assert_eq!(ShellKind::pick(None, true, Some(comspec)), ShellKind::Bash);
        assert_eq!(ShellKind::pick(None, false, Some(comspec)), ShellKind::Cmd);
        assert_eq!(
            ShellKind::pick(
                Some(Path::new(r"C:\Windows\System32\cmd.exe")),
                true,
                Some(comspec)
            ),
            ShellKind::Cmd,
            "a named shell is a choice, not a fallback"
        );
        assert_eq!(ShellKind::pick(None, false, None), ShellKind::Cmd);
    }
}
