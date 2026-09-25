use std::collections::HashSet;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Instant};
use tracing::{debug, info, warn};

use crate::config::AgyConfig;

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

const AGY_OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "response": { "type": "string" },
    "tool_calls": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "id": { "type": "string" },
          "name": { "type": "string" },
          "arguments": { "type": "object" }
        },
        "required": ["id", "name", "arguments"],
        "additionalProperties": false
      }
    }
  },
  "required": ["response", "tool_calls"],
  "additionalProperties": false
}"#;

#[derive(Debug, Clone)]
pub struct AgyCompletion {
    pub content: String,
    pub tool_calls: Vec<AgyToolCall>,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgyToolCall {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Deserialize)]
struct AgyStructuredOutput {
    #[serde(default)]
    response: String,
    #[serde(default)]
    tool_calls: Vec<AgyToolCall>,
}

struct AgyProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    allowed_init_tools: HashSet<String>,
    history: Vec<Value>,
}

struct AgyInner {
    process: Option<AgyProcess>,
}


#[derive(Debug)]
pub enum AgyStreamEvent {
    TextDelta(String),
    Heartbeat,
    Completed(AgyCompletion),
    Error(String),
}

#[derive(Debug, Default)]
struct JsonResponseStream {
    raw: String,
    response_start: Option<usize>,
    scan_pos: usize,
    escaped: bool,
    unicode_value: u16,
    unicode_digits: u8,
    pending_high_surrogate: Option<u16>,
    closed: bool,
    emitted: String,
}

impl JsonResponseStream {
    fn feed(&mut self, delta: &str) -> Result<String> {
        let before = self.emitted.len();
        self.raw.push_str(delta);
        self.locate_response_start();

        let Some(start) = self.response_start else {
            return Ok(String::new());
        };
        if self.closed {
            return Ok(String::new());
        }
        if self.scan_pos < start {
            self.scan_pos = start;
        }

        while self.scan_pos < self.raw.len() {
            let byte = self.raw.as_bytes()[self.scan_pos];

            if self.unicode_digits > 0 {
                let digit = (byte as char)
                    .to_digit(16)
                    .ok_or_else(|| anyhow!("AGY response JSON contains invalid unicode escape"))?
                    as u16;
                self.unicode_value = (self.unicode_value << 4) | digit;
                self.unicode_digits -= 1;
                self.scan_pos += 1;
                if self.unicode_digits == 0 {
                    self.finish_unicode_code_unit()?;
                }
                continue;
            }

            if self.escaped {
                self.escaped = false;
                match byte {
                    b'"' => self.emitted.push('"'),
                    b'\\' => self.emitted.push('\\'),
                    b'/' => self.emitted.push('/'),
                    b'b' => self.emitted.push('\u{0008}'),
                    b'f' => self.emitted.push('\u{000c}'),
                    b'n' => self.emitted.push('\n'),
                    b'r' => self.emitted.push('\r'),
                    b't' => self.emitted.push('\t'),
                    b'u' => {
                        self.unicode_value = 0;
                        self.unicode_digits = 4;
                    }
                    _ => {
                        return Err(anyhow!(
                            "AGY response JSON contains invalid escape sequence"
                        ));
                    }
                }
                self.scan_pos += 1;
                continue;
            }

            match byte {
                b'\\' => {
                    self.escaped = true;
                    self.scan_pos += 1;
                }
                b'"' => {
                    if let Some(high) = self.pending_high_surrogate.take() {
                        return Err(anyhow!(
                            "AGY response JSON ended with an unmatched high surrogate {:04x}",
                            high
                        ));
                    }
                    self.closed = true;
                    self.scan_pos += 1;
                }
                0x00..=0x1f => {
                    return Err(anyhow!(
                        "AGY response JSON contains an unescaped control character"
                    ));
                }
                _ => {
                    let ch = self.raw[self.scan_pos..]
                        .chars()
                        .next()
                        .ok_or_else(|| anyhow!("invalid UTF-8 in AGY response JSON"))?;
                    if self.pending_high_surrogate.is_some() {
                        return Err(anyhow!(
                            "AGY response JSON contains an invalid surrogate pair"
                        ));
                    }
                    self.emitted.push(ch);
                    self.scan_pos += ch.len_utf8();
                }
            }
        }

        Ok(self.emitted[before..].to_string())
    }

    fn locate_response_start(&mut self) {
        if self.response_start.is_some() {
            return;
        }
        let bytes = self.raw.as_bytes();
        let needle = b"\"response\"";
        let mut i = 0usize;
        while i + needle.len() <= bytes.len() {
            if &bytes[i..i + needle.len()] == needle {
                let mut j = i + needle.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b':' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'"' {
                        self.response_start = Some(j + 1);
                        self.scan_pos = j + 1;
                        return;
                    }
                }
            }
            i += 1;
        }
    }

    fn emitted(&self) -> &str {
        &self.emitted
    }

    fn finish_unicode_code_unit(&mut self) -> Result<()> {
        let unit = self.unicode_value;
        if (0xD800..=0xDBFF).contains(&unit) {
            if self.pending_high_surrogate.replace(unit).is_some() {
                return Err(anyhow!(
                    "AGY response JSON contains two consecutive high surrogates"
                ));
            }
            return Ok(());
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            let Some(high) = self.pending_high_surrogate.take() else {
                return Err(anyhow!(
                    "AGY response JSON contains a low surrogate without a high surrogate"
                ));
            };
            let scalar = 0x10000 + (((high as u32) - 0xD800) << 10) + ((unit as u32) - 0xDC00);
            let ch = char::from_u32(scalar)
                .ok_or_else(|| anyhow!("AGY response JSON contains an invalid surrogate pair"))?;
            self.emitted.push(ch);
            return Ok(());
        }
        if let Some(high) = self.pending_high_surrogate.take() {
            return Err(anyhow!(
                "AGY response JSON high surrogate {:04x} was not followed by a low surrogate",
                high
            ));
        }
        let ch = char::from_u32(unit as u32)
            .ok_or_else(|| anyhow!("AGY response JSON contains invalid unicode"))?;
        self.emitted.push(ch);
        Ok(())
    }
}

pub struct AgyProvider {
    cfg: AgyConfig,
    inner: Mutex<AgyInner>,
}

impl AgyProvider {
    pub fn new(cfg: AgyConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(AgyInner { process: None }),
        }
    }

    pub async fn complete(&self, payload: &Value) -> Result<AgyCompletion> {
        self.complete_inner(payload, None).await
    }

    pub fn stream(
        self: &std::sync::Arc<Self>,
        payload: &Value,
    ) -> mpsc::Receiver<AgyStreamEvent> {
        let (tx, rx) = mpsc::channel(64);
        let provider = std::sync::Arc::clone(self);
        let payload = payload.clone();

        tokio::spawn(async move {
            match provider.complete_inner(&payload, Some(&tx)).await {
                Ok(completion) => {
                    let _ = tx.send(AgyStreamEvent::Completed(completion)).await;
                }
                Err(error) => {
                    let _ = tx.send(AgyStreamEvent::Error(format!("{error:#}"))).await;
                }
            }
        });

        rx
    }

    async fn complete_inner(
        &self,
        payload: &Value,
        events: Option<&mpsc::Sender<AgyStreamEvent>>,
    ) -> Result<AgyCompletion> {
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| anyhow!("AGY backend requires a messages array"))?;

        if messages.is_empty() {
            return Err(anyhow!("AGY backend requires at least one message"));
        }

        let tools = payload
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tool_names = tool_names(&tools)?;

        let mut inner = self.inner.lock().await;

        if inner.process.is_none() {
            inner.process = Some(self.spawn_process().await?);
        }

        let restart = {
            let process = inner.process.as_ref().expect("AGY process must exist");
            !is_history_extension(&process.history, &messages)
        };

        if restart {
            stop_process(&mut inner).await;
            inner.process = Some(self.spawn_process().await?);
        }

        let delta = {
            let process = inner.process.as_ref().expect("AGY process must exist");
            history_delta(&process.history, &messages)
        };

        let effective_delta = if delta.is_empty() {
            stop_process(&mut inner).await;
            inner.process = Some(self.spawn_process().await?);
            messages.clone()
        } else {
            delta
        };

        let prompt = build_prompt(&effective_delta, &tools);
        send_user_event(
            inner
                .process
                .as_mut()
                .expect("AGY process must exist"),
            &prompt,
        )
        .await?;

        let result = match self
            .read_result(
                inner
                    .process
                    .as_mut()
                    .expect("AGY process must exist"),
                &tool_names,
                events,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                stop_process(&mut inner).await;
                return Err(error);
            }
        };

        inner
            .process
            .as_mut()
            .expect("AGY process must exist")
            .history = messages;

        Ok(result)
    }

    async fn spawn_process(&self) -> Result<AgyProcess> {
        let mut command = Command::new(&self.cfg.ssh_binary);
        command.args(&self.cfg.ssh_args);
        command.arg(&self.cfg.ssh_host);
        command.arg(self.build_remote_command());
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start SSH transport to {}", self.cfg.ssh_host))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("SSH transport did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("SSH transport did not expose stdout"))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => warn!("agy/ssh stderr: {line}"),
                        Ok(None) => break,
                        Err(error) => {
                            warn!("failed reading agy/ssh stderr: {error}");
                            break;
                        }
                    }
                }
            });
        }

        let mut process = AgyProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            allowed_init_tools: parse_allowed_tools(&self.cfg.allowed_init_tools),
            history: Vec::new(),
        };

        let init = timeout(self.cfg.timeout, read_json_line(&mut process.stdout))
            .await
            .context("timed out waiting for AGY init event")??;

        validate_init(&init, &process.allowed_init_tools)?;

        info!(
            host = %self.cfg.ssh_host,
            agent = %self.cfg.agent,
            remote_model = ?self.cfg.remote_model,
            "AGY persistent backend ready"
        );

        Ok(process)
    }

    fn build_remote_command(&self) -> String {
        let mut parts = vec![
            "cd".to_string(),
            shell_quote(&self.cfg.remote_cwd),
            "&&".to_string(),
            "exec".to_string(),
            shell_quote(&self.cfg.agy_path),
            shell_quote("--input-format"),
            shell_quote("stream-json"),
            shell_quote("--output-format"),
            shell_quote("stream-json"),
            shell_quote("--agent"),
            shell_quote(&self.cfg.agent),
            shell_quote("--json-schema"),
            shell_quote(AGY_OUTPUT_SCHEMA),
        ];

        if let Some(model) = self.cfg.remote_model.as_deref() {
            parts.push(shell_quote("--model"));
            parts.push(shell_quote(model));
        }

        if let Some(effort) = self.cfg.effort.as_deref() {
            parts.push(shell_quote("--effort"));
            parts.push(shell_quote(effort));
        }

        if self.cfg.print_timeout_seconds > 0 {
            parts.push(shell_quote("--print-timeout"));
            parts.push(shell_quote(&format!("{}s", self.cfg.print_timeout_seconds)));
        }

        parts.join(" ")
    }

    async fn read_result(
        &self,
        process: &mut AgyProcess,
        tool_names: &HashSet<String>,
        events: Option<&mpsc::Sender<AgyStreamEvent>>,
    ) -> Result<AgyCompletion> {
        let started = Instant::now();
        let mut response_stream = JsonResponseStream::default();

        loop {
            let elapsed = started.elapsed();
            if elapsed >= self.cfg.timeout {
                return Err(anyhow!("timed out waiting for AGY result"));
            }

            let remaining = self.cfg.timeout.saturating_sub(elapsed);
            let heartbeat = self.cfg.stream_heartbeat.min(remaining);

            match timeout(heartbeat, read_json_line(&mut process.stdout)).await {
                Ok(value) => {
                    let event = value.get("event").and_then(Value::as_str).unwrap_or("");

                    match event {
                        "step_update" => {
                            if let Some(step) = value.get("step_update") {
                                if step.get("step_type").and_then(Value::as_str) == Some("tool") {
                                    let name = step
                                        .get("tool_name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown");
                                    return Err(anyhow!(
                                        "AGY attempted internal tool {}; LLM-only backend is fail-closed",
                                        name
                                    ));
                                }

                                if step.get("step_type").and_then(Value::as_str)
                                    == Some("agent_response")
                                {
                                    if let Some(text_delta) =
                                        step.get("text_delta").and_then(Value::as_str)
                                    {
                                        let visible = response_stream.feed(text_delta)?;
                                        if !visible.is_empty() {
                                            if let Some(tx) = events {
                                                tx.send(AgyStreamEvent::TextDelta(visible))
                                                    .await
                                                    .map_err(|_| {
                                                        anyhow!("AGY client stream was dropped")
                                                    })?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        "result" => {
                            let completion =
                                parse_result(&value, tool_names, self.cfg.allow_text_fallback)?;

                            if let Some(tx) = events {
                                let streamed = response_stream.emitted();
                                if streamed.is_empty() && !completion.content.is_empty() {
                                    tx.send(AgyStreamEvent::TextDelta(completion.content.clone()))
                                        .await
                                        .map_err(|_| anyhow!("AGY client stream was dropped"))?;
                                } else if !completion.content.starts_with(streamed) {
                                    return Err(anyhow!(
                                        "AGY streamed response diverged from terminal result"
                                    ));
                                } else if completion.content.len() > streamed.len() {
                                    tx.send(AgyStreamEvent::TextDelta(
                                        completion.content[streamed.len()..].to_string(),
                                    ))
                                    .await
                                    .map_err(|_| anyhow!("AGY client stream was dropped"))?;
                                }
                            }

                            return Ok(completion);
                        }
                        "init" => {
                            return Err(anyhow!(
                                "AGY emitted a second init event during a turn"
                            ));
                        }
                        "error" => {
                            return Err(anyhow!("AGY returned an error event: {value}"));
                        }
                        other => {
                            debug!(event = other, "ignoring AGY stream event");
                        }
                    }
                }
                Err(_) => {
                    if let Some(tx) = events {
                        tx.send(AgyStreamEvent::Heartbeat)
                            .await
                            .map_err(|_| anyhow!("AGY client stream was dropped"))?;
                    }
                }
            }
        }
    }

}

async fn stop_process(inner: &mut AgyInner) {
    if let Some(mut process) = inner.process.take() {
        let _ = process.stdin.shutdown().await;
        let _ = process.child.kill().await;
        let _ = process.child.wait().await;
    }
}

async fn send_user_event(process: &mut AgyProcess, prompt: &str) -> Result<()> {
    let event = json!({
        "event": "user",
        "message": {
            "content": prompt
        }
    });

    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    process.stdin.write_all(&line).await?;
    process.stdin.flush().await?;
    Ok(())
}

fn parse_result(
    value: &Value,
    tool_names: &HashSet<String>,
    allow_text_fallback: bool,
) -> Result<AgyCompletion> {
    let result = value
        .get("result")
        .ok_or_else(|| anyhow!("AGY result event has no result object"))?;

    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");

    if matches!(status, "ERROR" | "INVALID" | "CANCELED") {
        let message = result
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| result.get("response").and_then(Value::as_str))
            .unwrap_or("unknown AGY error");
        return Err(anyhow!("AGY turn failed with status {status}: {message}"));
    }

    let structured = result
        .get("structured_output")
        .and_then(|v| serde_json::from_value::<AgyStructuredOutput>(v.clone()).ok())
        .or_else(|| {
            result
                .get("response")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<AgyStructuredOutput>(text).ok())
        });

    let (content, mut tool_calls) = match structured {
        Some(value) => (value.response, value.tool_calls),
        None if allow_text_fallback => (
            result
                .get("response")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Vec::new(),
        ),
        None => {
            return Err(anyhow!(
                "AGY did not return the required structured_output object"
            ))
        }
    };

    for call in &mut tool_calls {
        if !tool_names.contains(&call.name) {
            return Err(anyhow!(
                "AGY requested unknown tool {}; refusing the tool call",
                call.name
            ));
        }
        if !call.arguments.is_object() {
            return Err(anyhow!(
                "AGY tool {} returned non-object arguments",
                call.name
            ));
        }
        call.id = next_tool_call_id();
    }

    Ok(AgyCompletion {
        content,
        tool_calls,
        usage: result.get("usage").cloned(),
    })
}

fn validate_init(value: &Value, allowed_tools: &HashSet<String>) -> Result<()> {
    if value.get("event").and_then(Value::as_str) != Some("init") {
        return Err(anyhow!("expected AGY init event, got: {value}"));
    }

    let tools = value
        .get("init")
        .and_then(|v| v.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for tool in tools {
        let name = tool
            .as_str()
            .ok_or_else(|| anyhow!("AGY init contains a non-string tool name"))?;

        if !allowed_tools.contains(name) {
            return Err(anyhow!(
                "AGY agent exposes tool {}; refused in LLM-only mode",
                name
            ));
        }
    }

    Ok(())
}

fn tool_names(tools: &[Value]) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for tool in tools {
        let name = tool
            .pointer("/function/name")
            .and_then(Value::as_str)
            .or_else(|| tool.get("name").and_then(Value::as_str))
            .ok_or_else(|| anyhow!("OpenAI tool is missing function.name"))?;
        names.insert(name.to_string());
    }
    Ok(names)
}

fn build_prompt(delta: &[Value], tools: &[Value]) -> String {
    let tools_json = serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_string());
    let messages_json = serde_json::to_string(delta).unwrap_or_else(|_| "[]".to_string());

    format!(
        "You are the LLM backend behind an OpenAI-compatible router.\n\
         You are NOT the executor and must not use host tools.\n\
         Never run commands, edit files, browse, call MCP, or perform side effects.\n\
         The caller executes any tool call you return.\n\
         Return ONLY the JSON object required by the output schema.\n\
         The response field is normal assistant text.\n\
         The tool_calls field contains calls to external tools listed below.\n\
         Use an empty tool_calls array when no tool is needed.\n\
         Tool call arguments must be a JSON object matching the selected tool schema.\n\
         Do not invent tool names.\n\
         External tools for this turn:\n{}\n\
         Conversation messages added since the previous turn:\n{}\n",
        tools_json, messages_json
    )
}

fn history_delta(previous: &[Value], current: &[Value]) -> Vec<Value> {
    let prefix = common_prefix_len(previous, current);
    current[prefix..].to_vec()
}

fn common_prefix_len(left: &[Value], right: &[Value]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn is_history_extension(previous: &[Value], current: &[Value]) -> bool {
    previous.len() <= current.len()
        && previous
            .iter()
            .zip(current.iter())
            .all(|(a, b)| a == b)
}

fn next_tool_call_id() -> String {
    let seq = NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed);
    format!("call_agy_{seq}")
}

fn parse_allowed_tools(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn read_json_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let bytes = stdout
        .read_line(&mut line)
        .await
        .context("failed to read AGY stdout")?;

    if bytes == 0 {
        return Err(anyhow!("AGY stdout closed unexpectedly"));
    }

    serde_json::from_str(line.trim())
        .with_context(|| format!("AGY emitted invalid JSON: {}", line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_stream_extracts_incremental_json_string() {
        let mut stream = JsonResponseStream::default();
        assert_eq!(stream.feed(r#"{"response":"Hel"#).unwrap(), "Hel");
        assert_eq!(stream.feed(r#"lo\nworld"}"#).unwrap(), "lo\nworld");
        assert_eq!(stream.emitted(), "Hello\nworld");
    }

    #[test]
    fn response_stream_decodes_surrogate_pair() {
        let mut stream = JsonResponseStream::default();
        assert_eq!(stream.feed(r#"{"response":"\uD83D"#).unwrap(), "");
        assert_eq!(stream.feed(r#"\uDE80"}"#).unwrap(), "🚀");
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("agy"), "'agy'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn history_extension_and_delta_work() {
        let previous = vec![json!({"role":"user","content":"one"})];
        let current = vec![
            json!({"role":"user","content":"one"}),
            json!({"role":"assistant","content":"two"}),
        ];

        assert!(is_history_extension(&previous, &current));
        assert_eq!(history_delta(&previous, &current), vec![json!({"role":"assistant","content":"two"})]);
    }

    #[test]
    fn divergent_history_is_rejected() {
        let previous = vec![json!({"role":"user","content":"one"})];
        let current = vec![json!({"role":"user","content":"different"})];
        assert!(!is_history_extension(&previous, &current));
    }

    #[test]
    fn parse_result_normalizes_tool_call_id() {
        let value = json!({
            "event":"result",
            "result":{
                "status":"SUCCESS",
                "structured_output":{
                    "response":"",
                    "tool_calls":[
                        {
                            "id":"model-id",
                            "name":"read_file",
                            "arguments":{"path":"README.md"}
                        }
                    ]
                }
            }
        });
        let allowed = HashSet::from(["read_file".to_string()]);
        let result = parse_result(&value, &allowed, false).unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_ne!(result.tool_calls[0].id, "model-id");
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let value = json!({
            "event":"result",
            "result":{
                "status":"SUCCESS",
                "structured_output":{
                    "response":"",
                    "tool_calls":[
                        {"id":"x","name":"unknown","arguments":{}}
                    ]
                }
            }
        });

        assert!(parse_result(&value, &HashSet::new(), false).is_err());
    }

    #[test]
    fn init_validation_is_fail_closed() {
        let value = json!({
            "event":"init",
            "init":{"tools":["run_command"]}
        });

        assert!(validate_init(&value, &HashSet::new()).is_err());
    }

    #[test]
    fn prompt_contains_tools_and_delta() {
        let prompt = build_prompt(
            &[json!({"role":"user","content":"hello"})],
            &[json!({
                "type":"function",
                "function":{
                    "name":"read_file",
                    "parameters":{"type":"object"}
                }
            })],
        );

        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("hello"));
        assert!(prompt.contains("NOT the executor"));
    }
}
