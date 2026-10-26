# Security Policy

## Supported Versions

sqwai is under active development. Security updates are applied directly to the latest `master` branch and tagged releases.

| Version | Supported |
| :--- | :--- |
| `master` / Latest Release | :white_check_mark: |
| Older Versions | :x: |

## Reporting a Vulnerability

If you discover a potential security vulnerability in sqwai, please do not open a public issue. Report it confidentially through either of the following channels:

- **Email:** [remdima750@gmail.com](mailto:remdima750@gmail.com)
- **Telegram:** [@hksae](https://t.me/hksae)

You may also use GitHub's private vulnerability reporting via the repository's **Security > Report a vulnerability** tab.

Please include in your report:
- A description of the vulnerability and its potential impact.
- Step-by-step reproduction instructions or a minimal proof-of-concept.
- The environment details (OS, shell, sqwai commit or version).

## Security Model & Scope

sqwai executes shell commands, manipulates project files, and interacts with external model APIs and MCP servers. Security concerns include, but are not limited to:

1. **Command Safety & Trust Gate Bypasses:** Circumvention of the shell command safety classifier (`src/agent/safety.rs`), blocked command patterns, or the taint-aware egress trust gate (`src/agent/trust.rs`), enabling destructive commands or network exfiltration without user approval.
2. **Credential & Secret Leaks:** Exposure of provider API keys or tokens in session transcripts, event tails (`.sqwai/events/`), or tool outputs (`src/agent/secrets.rs`).
3. **Prompt Injection & Untrusted Content Escalation:** Vulnerabilities where untrusted repository files, command output, web responses (`webfetch` / `websearch`), or MCP server payloads bypass untrusted-content boundaries or project config allowlists (`.sqwai/config.toml`).
4. **Workspace Jail & Host-Owned State Bypass:** Path traversal escaping the project root, subagent write-scope violations (`src/agent/tools/policy.rs`), or direct modification of host-owned state (`.sqwai/plans/`, `.sqwai/events/`, `.sqwai/checkpoints/`) outside dedicated tools.
5. **SSRF & Local Network Access:** Bypassing loopback/private-network restrictions in `webfetch` (`src/agent/tools/web.rs`) when not explicitly permitted by `[web].allow_hosts`.

