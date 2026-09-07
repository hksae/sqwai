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

sqwai executes shell commands, manipulates project files, and interacts with external model APIs. Security concerns include, but are not limited to:

1. **Safety Classifier Bypasses:** Circumvention of the two-layer command safety filter (`src/agent/safety.rs`), enabling execution of destructive commands without user approval.
2. **Credential & Secret Leaks:** Exposure of provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) in session logs, event journals, or shell outputs.
3. **Prompt Injection / Untrusted Content Escalation:** Vulnerabilities where untrusted file contents or tool outputs coerce the agent into unauthorized filesystem operations or secret exfiltration.
4. **Path Traversal:** File access or modifications escaping the designated project workspace without user consent.
