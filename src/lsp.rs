//! Minimal LSP JSON-RPC framing and diagnostics types.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_MESSAGE_BYTES {
        bail!("LSP message is too large");
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut length = None;
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            bail!("LSP server closed stdout");
        }
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            bail!("invalid LSP header");
        };
        let (key, value) = (&line[..colon], &line[colon + 1..]);
        if key.eq_ignore_ascii_case(b"Content-Length") {
            length = Some(std::str::from_utf8(value)?.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing Content-Length")?;
    if length > MAX_MESSAGE_BYTES {
        bail!("LSP message is too large");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    pub async fn spawn(server: &crate::config::LspServerDef, root: &Path) -> Result<Self> {
        let mut child = Command::new(&server.command)
            .args(&server.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("start LSP server {}", server.name))?;
        let stdin = child.stdin.take().context("LSP stdin unavailable")?;
        let stdout = child.stdout.take().context("LSP stdout unavailable")?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
            next_id: 1,
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(
            &mut self.stdin,
            &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await?;
        loop {
            let msg = read_message(&mut self.stdout).await?;
            if msg.get("id") == Some(&Value::from(id)) {
                if let Some(error) = msg.get("error") {
                    bail!("LSP {method}: {error}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    pub async fn initialize(&mut self, root: &Path) -> Result<Value> {
        let uri = file_uri(root)?;
        let result = self.request("initialize", serde_json::json!({
            "processId": std::process::id(), "clientInfo": {"name":"sqwai", "version":env!("CARGO_PKG_VERSION")},
            "rootUri": uri, "capabilities": {"textDocument": {"synchronization": {"dynamicRegistration": false, "willSave": false, "didSave": true}}}
        })).await?;
        write_message(
            &mut self.stdin,
            &serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )
        .await?;
        Ok(result)
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        write_message(
            &mut self.stdin,
            &serde_json::json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
        .await
    }

    pub async fn did_open(&mut self, path: &Path, language_id: &str, text: &str) -> Result<()> {
        self.notify("textDocument/didOpen", serde_json::json!({
            "textDocument": { "uri": file_uri(path)?, "languageId": language_id, "version": 1, "text": text }
        })).await
    }

    pub async fn did_change(&mut self, path: &Path, version: i32, text: &str) -> Result<()> {
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": file_uri(path)?, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    pub async fn did_save(&mut self, path: &Path) -> Result<()> {
        self.notify(
            "textDocument/didSave",
            serde_json::json!({
                "textDocument": { "uri": file_uri(path)? }
            }),
        )
        .await
    }

    pub async fn next_diagnostics(&mut self) -> Result<Option<PublishDiagnosticsParams>> {
        loop {
            let msg = read_message(&mut self.stdout).await?;
            if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            {
                return Ok(Some(serde_json::from_value(
                    msg.get("params").cloned().unwrap_or(Value::Null),
                )?));
            }
        }
    }
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null).await?;
        self.notify("exit", Value::Null).await?;
        let _ = self.child.wait().await?;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub range: Range,
    pub message: String,
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub source: Option<String>,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    #[serde(default)]
    pub version: Option<i32>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn file_uri(path: &Path) -> Result<String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut s = path.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let mut out = String::from("file://");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn frames_messages_by_utf8_bytes() {
        let value = serde_json::json!({"method":"x", "params":"Привет"});
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).await.unwrap();
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(read_message(&mut reader).await.unwrap(), value);
    }

    #[test]
    fn builds_file_uri() {
        let uri = file_uri(Path::new("C:\\work dir\\main.rs")).unwrap();
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("%20"));
    }
}
