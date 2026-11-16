//! Optional AI help: "Explain" on a resource and "Summarize" on logs.
//!
//! Off by default. The app talks directly to the provider the user picked, with the user's own key
//! (or to a local model through Ollama or LM Studio); nothing goes through steerglass.com. Secrets are
//! never read, and sensitive-looking values are redacted before anything is sent. The UI can show
//! the exact text before sending it (`ai_prepare`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{Api, ListParams, LogParams};
use kube::Client;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::commands::dyn_api;
use crate::error::{Error, Result};
use crate::state::{AppState, KindRef, StreamHandle};

// ---------------------------------------------------------------- settings

/// AI settings, saved with the other settings. The API key is not here: it lives in the system
/// keychain (see `read_key`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AiSettings {
    pub enabled: bool,
    /// openai, anthropic, ollama, lmstudio, deepseek, mistral, openrouter or custom.
    pub provider: String,
    /// Empty: the provider's usual address.
    pub base_url: String,
    pub model: String,
    /// Also allowed on clusters marked as production (checked by the UI, which knows environments).
    pub on_production: bool,
    /// The language of the answers, e.g. "Italian"; empty: English.
    pub language: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self { enabled: false, provider: "openai".into(), base_url: String::new(), model: String::new(), on_production: false, language: String::new() }
    }
}

/// The usual API address of each provider; `None` for "custom", which needs one.
pub fn default_base(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "ollama" => "http://localhost:11434/v1",
        "lmstudio" => "http://localhost:1234/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        _ => return None,
    })
}

/// Local servers don't need a key.
fn needs_key(provider: &str) -> bool {
    !matches!(provider, "ollama" | "lmstudio" | "custom")
}

impl AiSettings {
    /// The system prompt with the answer language added.
    pub(crate) fn system(&self, prompt: &str) -> String {
        match self.language.trim() {
            "" | "English" => prompt.to_string(),
            lang => format!("{prompt}\n\nAlways answer in {lang}, whatever the language of the data. Keep Kubernetes terms, field names, commands and log lines as they are."),
        }
    }

    pub(crate) fn base(&self) -> Result<String> {
        let base = if self.base_url.trim().is_empty() { default_base(&self.provider).unwrap_or("") } else { self.base_url.trim() };
        if base.is_empty() {
            return Err(Error::msg("Set the server address in Settings → AI."));
        }
        Ok(base.trim_end_matches('/').to_string())
    }
}

// ---------------------------------------------------------------- API keys

const KEYCHAIN_SERVICE: &str = "com.steerglass.app";
const KEY_FILE: &str = "ai-keys.json";

/// Keys already read, so the keychain is asked once per provider.
static KEYS: LazyLock<Mutex<BTreeMap<String, Option<String>>>> = LazyLock::new(Default::default);

fn keychain(provider: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("ai-key-{provider}"))
}

/// Fallback when there is no keychain (Linux without a secret service): a file only the user can read.
fn key_file(dir: &Path) -> PathBuf {
    dir.join(KEY_FILE)
}

fn file_keys(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read(key_file(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_file_keys(dir: &Path, keys: &BTreeMap<String, String>) -> std::io::Result<()> {
    let path = key_file(dir);
    if keys.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&path)?, &serde_json::to_vec(keys)?)
}

fn config_dir(state: &AppState) -> PathBuf {
    state.settings_path.parent().map(Path::to_path_buf).unwrap_or_default()
}

pub fn read_key(state: &AppState, provider: &str) -> Option<String> {
    if let Some(k) = KEYS.lock().get(provider) {
        return k.clone();
    }
    let key = keychain(provider)
        .and_then(|e| e.get_password())
        .ok()
        .or_else(|| file_keys(&config_dir(state)).remove(provider))
        .filter(|k| !k.is_empty());
    KEYS.lock().insert(provider.to_string(), key.clone());
    key
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyStatus {
    pub set: bool,
    /// "keychain" or "file" when set.
    pub stored_in: Option<&'static str>,
}

fn key_status(state: &AppState, provider: &str) -> KeyStatus {
    let set = read_key(state, provider).is_some();
    let in_file = file_keys(&config_dir(state)).contains_key(provider);
    KeyStatus { set, stored_in: set.then_some(if in_file { "file" } else { "keychain" }) }
}

#[tauri::command]
pub fn ai_key_status(state: State<'_, AppState>, provider: String) -> KeyStatus {
    key_status(&state, &provider)
}

/// Saves the key in the system keychain (a private file when there is none); an empty key removes it.
#[tauri::command]
pub fn ai_set_key(state: State<'_, AppState>, provider: String, key: String) -> Result<KeyStatus> {
    let key = key.trim();
    let dir = config_dir(&state);
    let mut file = file_keys(&dir);
    file.remove(&provider);
    if key.is_empty() {
        if let Ok(e) = keychain(&provider) {
            let _ = e.delete_credential();
        }
    } else if keychain(&provider).and_then(|e| e.set_password(key)).is_err() {
        file.insert(provider.clone(), key.to_string());
    }
    save_file_keys(&dir, &file)?;
    KEYS.lock().insert(provider.clone(), (!key.is_empty()).then(|| key.to_string()));
    Ok(key_status(&state, &provider))
}

#[tauri::command]
pub fn ai_update_settings(state: State<'_, AppState>, ai: AiSettings) -> Result<()> {
    state.settings.write().ai = ai;
    state.save_settings()
}

// ---------------------------------------------------------------- HTTP

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("Steerglass/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // Local models can take a while to load before the first word.
        .read_timeout(Duration::from_secs(180))
        .build()
        .expect("HTTP client")
});

pub(crate) fn net_error(ai: &AiSettings, base: &str, e: reqwest::Error) -> Error {
    let hint = match ai.provider.as_str() {
        "ollama" => " Is Ollama running?",
        "lmstudio" => " Is the LM Studio server started?",
        _ => "",
    };
    Error::msg(format!("Cannot reach {base}: {}.{hint}", root_cause(&e)))
}

fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

pub(crate) fn api_error(status: reqwest::StatusCode, body: &str) -> Error {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().or(v["error"].as_str()).or(v["message"].as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(300).collect());
    Error::msg(match status.as_u16() {
        401 | 403 => format!("The provider refused the API key: {detail} Check it in Settings → AI."),
        404 => format!("Not found: {detail} Check the model and the server address in Settings → AI."),
        429 => format!("Rate limit or quota reached: {detail}"),
        _ => format!("The provider answered {status}: {detail}"),
    })
}

pub(crate) fn request(ai: &AiSettings, key: Option<&str>, method: reqwest::Method, path: &str) -> Result<(String, reqwest::RequestBuilder)> {
    let base = ai.base()?;
    let mut req = HTTP.request(method, format!("{base}{path}"));
    if ai.provider == "anthropic" {
        req = req.header("x-api-key", key.unwrap_or_default()).header("anthropic-version", "2023-06-01");
    } else if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    Ok((base, req))
}

pub(crate) fn key_for(state: &AppState, ai: &AiSettings) -> Result<Option<String>> {
    let key = read_key(state, &ai.provider);
    if key.is_none() && needs_key(&ai.provider) {
        return Err(Error::msg("Add your API key in Settings → AI."));
    }
    Ok(key)
}

/// The models the server offers (also a connection test). Settings that aren't saved yet can be tried.
#[tauri::command]
pub async fn ai_models(state: State<'_, AppState>, ai: AiSettings) -> Result<Vec<String>> {
    let key = key_for(&state, &ai)?;
    // Anthropic lists 20 models per page unless asked for more.
    let path = if ai.provider == "anthropic" { "/models?limit=1000" } else { "/models" };
    let (base, req) = request(&ai, key.as_deref(), reqwest::Method::GET, path)?;
    let resp = req.send().await.map_err(|e| net_error(&ai, &base, e))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| net_error(&ai, &base, e))?;
    if !status.is_success() {
        return Err(api_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body)?;
    // Skip models that can't chat (embeddings, speech, images…), listed by OpenAI and others.
    let skip = Regex::new(r"(?i)embed|tts|whisper|dall-e|moderation|image|audio|realtime|transcribe|speech|rerank").unwrap();
    let mut models: Vec<String> = v["data"]
        .as_array()
        .or(v["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().or(m["name"].as_str()).map(String::from))
        .filter(|m| !skip.is_match(m))
        .collect();
    models.sort();
    models.dedup();
    Ok(models)
}

// ---------------------------------------------------------------- tasks

const EXPLAIN_PROMPT: &str = "You are an expert Kubernetes site reliability engineer built into Steerglass, a desktop app for \
operating Kubernetes clusters. The user clicked \"Explain\" on a resource. You get its current state as YAML (sensitive values \
are redacted), its recent events and, when useful, recent container logs and the pods it manages.

Answer in Markdown, short and concrete:
- Start with one line in bold: the diagnosis. What is wrong, or, when nothing is, that it looks healthy and what it does.
- **Why**: the most likely cause, quoting the event or log line that shows it. When several causes are possible, rank them.
- **What to do**: numbered steps. Show kubectl commands or YAML changes in code blocks when they help. Never suggest deleting \
data, volumes or namespaces as a fix.
Use only the facts in the data and general Kubernetes knowledge. When something important is missing, say what to check next. \
No preamble and no closing remarks.";

const LOGS_PROMPT: &str = "You are an expert Kubernetes site reliability engineer built into Steerglass, a desktop app for \
operating Kubernetes clusters. The user asked for a summary of a container's logs (sensitive values are redacted).

Answer in Markdown, short and concrete:
- Start with one line in bold: what the program is doing and whether it is healthy.
- **Errors**: group repeated errors and warnings, with how many times each appears and when it first appears. The first error \
is often the cause of the ones after it.
- **What to check**: numbered next steps, if anything looks wrong.
Use only what is in the logs. No preamble and no closing remarks.";

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AiTask {
    /// Explain a resource: its state, events, and logs or pods when useful.
    #[serde(rename_all = "camelCase")]
    Explain { context: String, kind: KindRef, namespace: Option<String>, name: String },
    /// Summarize log lines the UI already has.
    #[serde(rename_all = "camelCase")]
    Logs { pod: String, container: Option<String>, lines: Vec<String> },
}

/// The message sent for a task (the system prompt aside): what the user can review before sending.
async fn prepare(state: &AppState, task: &AiTask) -> Result<(&'static str, String)> {
    Ok(match task {
        AiTask::Explain { context, kind, namespace, name } => {
            let body = gather(state, context, kind, namespace.as_deref(), name).await?;
            let ns = namespace.as_deref().filter(|n| !n.is_empty()).map(|n| format!(" in namespace `{n}`")).unwrap_or_default();
            (EXPLAIN_PROMPT, redact(&format!("Explain the {} `{name}`{ns}. Current time: {}.\n\n{body}", kind.kind, now())))
        }
        AiTask::Logs { pod, container, lines } => {
            let mut text = String::new();
            let mut total = 0;
            let mut kept: Vec<String> = Vec::new();
            for l in lines.iter().rev() {
                let l = clip_line(l, 500);
                total += l.len() + 1;
                if total > 40_000 || kept.len() >= 600 {
                    break;
                }
                kept.push(l);
            }
            kept.reverse();
            let c = container.as_deref().map(|c| format!(" of container `{c}`")).unwrap_or_default();
            text.push_str(&format!(
                "Summarize the logs{c} in pod `{pod}`: the last {} lines{}, oldest first. Current time: {}.\n\n```\n",
                kept.len(),
                if kept.len() < lines.len() { format!(" of {}", lines.len()) } else { String::new() },
                now()
            ));
            text.push_str(&kept.join("\n"));
            text.push_str("\n```\n");
            (LOGS_PROMPT, redact(&text))
        }
    })
}

pub(crate) fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub text: String,
    /// Where it would go: "Anthropic · claude-…".
    pub destination: String,
}

pub(crate) fn destination(ai: &AiSettings) -> String {
    let host = ai.base().ok().and_then(|b| b.split("://").nth(1).map(|h| h.split('/').next().unwrap_or(h).to_string())).unwrap_or_default();
    format!("{host} · {}", if ai.model.is_empty() { "no model chosen" } else { &ai.model })
}

/// Collects what would be sent, without sending it.
#[tauri::command]
pub async fn ai_prepare(state: State<'_, AppState>, task: AiTask) -> Result<Prepared> {
    let ai = state.settings.read().ai.clone();
    let (_, text) = prepare(&state, &task).await?;
    Ok(Prepared { text, destination: destination(&ai) })
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AiMsg {
    /// The text sent, and where.
    Sent { text: String, destination: String },
    Delta { text: String },
    Done,
    Error { message: String },
}

/// Collects the data, sends it and streams the answer back. Stop it with `stream_stop`.
#[tauri::command]
pub async fn ai_run(app: AppHandle, state: State<'_, AppState>, task: AiTask, on_event: Channel<AiMsg>) -> Result<u64> {
    let ai = state.settings.read().ai.clone();
    if !ai.enabled {
        return Err(Error::msg("AI is off. Turn it on in Settings → AI."));
    }
    if ai.model.trim().is_empty() {
        return Err(Error::msg("Choose a model in Settings → AI."));
    }
    let key = key_for(&state, &ai)?;
    let (system, text) = prepare(&state, &task).await?;
    let id = state.next_id();
    let app2 = app.clone();
    let task = tokio::spawn(async move {
        let _ = on_event.send(AiMsg::Sent { text: text.clone(), destination: destination(&ai) });
        let msg = match stream_answer(&ai, key.as_deref(), &ai.system(system), &text, &on_event).await {
            Ok(()) => AiMsg::Done,
            Err(e) => AiMsg::Error { message: e.to_string() },
        };
        let _ = on_event.send(msg);
        crate::streams::unregister(&app2, id);
    });
    crate::streams::register(&app, id, StreamHandle { abort: task.abort_handle(), stdin: None, resize: None, forward: None });
    Ok(id)
}

/// One answer, not streamed (short tasks: a command, a navigation).
pub(crate) async fn complete(ai: &AiSettings, key: Option<&str>, system: &str, user: &str, max_tokens: u32) -> Result<String> {
    let anthropic = ai.provider == "anthropic";
    let body = if anthropic {
        json!({ "model": ai.model, "max_tokens": max_tokens, "system": system, "messages": [{ "role": "user", "content": user }] })
    } else {
        json!({ "model": ai.model, "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }] })
    };
    let path = if anthropic { "/messages" } else { "/chat/completions" };
    let (base, req) = request(ai, key, reqwest::Method::POST, path)?;
    let resp = req.json(&body).send().await.map_err(|e| net_error(ai, &base, e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| net_error(ai, &base, e))?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let out = if anthropic {
        v["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<String>()
    } else {
        v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string()
    };
    // Reasoning models may think aloud first.
    let out = match out.rfind("</think>") {
        Some(i) => out[i + 8..].to_string(),
        None => out,
    };
    Ok(out.trim().to_string())
}

/// The settings and key when AI can be used, or why not.
pub(crate) fn ready(state: &AppState) -> Result<(AiSettings, Option<String>)> {
    let ai = state.settings.read().ai.clone();
    if !ai.enabled {
        return Err(Error::msg("AI is off. Turn it on in Settings → AI."));
    }
    if ai.model.trim().is_empty() {
        return Err(Error::msg("Choose a model in Settings → AI."));
    }
    let key = key_for(state, &ai)?;
    Ok((ai, key))
}

async fn stream_answer(ai: &AiSettings, key: Option<&str>, system: &str, user: &str, on: &Channel<AiMsg>) -> Result<()> {
    let anthropic = ai.provider == "anthropic";
    let body = if anthropic {
        json!({ "model": ai.model, "max_tokens": 2048, "stream": true, "system": system, "messages": [{ "role": "user", "content": user }] })
    } else {
        json!({ "model": ai.model, "stream": true, "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }] })
    };
    let path = if anthropic { "/messages" } else { "/chat/completions" };
    let (base, req) = request(ai, key, reqwest::Method::POST, path)?;
    let resp = req.json(&body).send().await.map_err(|e| net_error(ai, &base, e))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(api_error(status, &body));
    }
    // A server that ignores "stream" answers with one JSON object.
    let is_json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        let v: Value = resp.json().await.map_err(|e| net_error(ai, &base, e))?;
        let text = if anthropic { v["content"][0]["text"].as_str() } else { v["choices"][0]["message"]["content"].as_str() };
        let _ = on.send(AiMsg::Delta { text: text.unwrap_or_default().to_string() });
        return Ok(());
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| net_error(ai, &base, e))?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(());
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(msg) = v["error"]["message"].as_str() {
                return Err(Error::msg(msg.to_string()));
            }
            let text = if anthropic {
                if v["type"] == "message_stop" {
                    return Ok(());
                }
                v["delta"]["text"].as_str()
            } else {
                v["choices"][0]["delta"]["content"].as_str()
            };
            if let Some(t) = text.filter(|t| !t.is_empty())
                && on.send(AiMsg::Delta { text: t.to_string() }).is_err()
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- what is sent

const MAX_SPEC: usize = 8_000;
const MAX_STATUS: usize = 6_000;
const MAX_EVENTS: usize = 25;
const LOG_LINES: i64 = 60;

fn section(out: &mut String, title: &str, body: &str, lang: &str) {
    out.push_str(&format!("## {title}\n\n```{lang}\n{}\n```\n\n", body.trim_end()));
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut, {} more characters)", &s[..end], s.len() - end)
}

pub(crate) fn clip_line(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

static SENSITIVE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)pass|secret|token|key|credential|auth|private|cert|dsn|salt|cookie|session").unwrap());

/// Drops what is useless or risky to send: managed fields, last-applied copies, node image lists,
/// binary data, and the values of environment variables with sensitive names.
pub(crate) fn sanitize(v: &mut Value) {
    if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.remove("managedFields");
        if let Some(a) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
        }
    }
    if let Some(status) = v.get_mut("status").and_then(Value::as_object_mut) {
        status.remove("images");
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("binaryData");
    }
    walk(v);
}

fn walk(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if let Some(Value::Array(env)) = o.get_mut("env") {
                for e in env.iter_mut() {
                    let sensitive = e["name"].as_str().is_some_and(|n| SENSITIVE_NAME.is_match(n));
                    if sensitive && e.get("value").is_some() {
                        e["value"] = json!("[redacted]");
                    }
                }
            }
            for (_, child) in o.iter_mut() {
                walk(child);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(walk),
        Value::String(s) if s.len() > 2_000 => *s = clip(s, 2_000),
        _ => {}
    }
}

static REDACTIONS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "[redacted private key]"),
        (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "[redacted token]"),
        (r"(?i)\b(bearer|basic)\s+[A-Za-z0-9\-._~+/]{8,}=*", "$1 [redacted]"),
        (r"\b(AKIA|ASIA)[0-9A-Z]{16}\b", "[redacted key]"),
        (r"\b(sk|pk|rk)[-_](live|test|proj|ant)?[-_]?[A-Za-z0-9_-]{16,}", "[redacted key]"),
        (r"\b(ghp|gho|ghs|ghu|github_pat|glpat|xox[abpr])[-_][A-Za-z0-9_-]{10,}", "[redacted token]"),
        (r"([a-zA-Z][a-zA-Z0-9+.-]*://[^/\s:@]+):[^/\s@]+@", "$1:[redacted]@"),
        (
            r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|secret[_-]?key|client[_-]?secret|authorization|private[_-]?key)(["']?[ \t]*[:=][ \t]*["']?)([^\s"',;&]+)"#,
            "$1$2[redacted]",
        ),
    ]
    .into_iter()
    .map(|(re, with)| (Regex::new(re).unwrap(), with))
    .collect()
});

/// Hides values that look like credentials in free text (logs, YAML, events).
pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for (re, with) in REDACTIONS.iter() {
        if re.is_match(&out) {
            out = re.replace_all(&out, *with).into_owned();
        }
    }
    out
}

async fn gather(state: &AppState, context: &str, kind: &KindRef, namespace: Option<&str>, name: &str) -> Result<String> {
    if kind.group.is_empty() && kind.kind == "Secret" {
        return Err(Error::msg("Secrets are never sent to an AI provider."));
    }
    let client = state.client(context).await?;
    let obj = dyn_api(client.clone(), kind, namespace).get(name).await?;
    let mut v = serde_json::to_value(&obj)?;
    sanitize(&mut v);
    let mut out = String::new();

    let status = v.as_object_mut().and_then(|o| o.remove("status"));
    section(&mut out, &format!("{} (YAML, without status)", kind.kind), &clip(&serde_yaml::to_string(&v)?, MAX_SPEC), "yaml");
    if let Some(s) = status.filter(|s| s.as_object().is_some_and(|o| !o.is_empty())) {
        section(&mut out, "Status", &clip(&serde_yaml::to_string(&s)?, MAX_STATUS), "yaml");
        v["status"] = s;
    }

    let ns = namespace.filter(|n| !n.is_empty());
    section(&mut out, "Recent events", &events_for(&client, &kind.kind, ns, name).await, "");

    let is_pod = kind.group.is_empty() && kind.kind == "Pod";
    if is_pod {
        if let Some(ns) = ns {
            pod_logs(&client, ns, name, &v, 3, &mut out).await;
        }
    } else if let (Some(ns), Some(sel)) = (ns, selector(kind, &v)) {
        managed_pods(&client, ns, &sel, &mut out).await;
    }
    Ok(out)
}

pub(crate) async fn events_for(client: &Client, kind: &str, namespace: Option<&str>, name: &str) -> String {
    let api: Api<Event> = match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    };
    let lp = ListParams::default().fields(&format!("involvedObject.name={name},involvedObject.kind={kind}"));
    let list = match api.list(&lp).await {
        Ok(l) => l,
        Err(e) => return format!("(events could not be read: {})", crate::error::Error::from(e)),
    };
    let mut events: Vec<(String, String)> = list
        .items
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .map(|e| {
            let at = e["lastTimestamp"]
                .as_str()
                .or(e["eventTime"].as_str())
                .or(e["metadata"]["creationTimestamp"].as_str())
                .unwrap_or("")
                .to_string();
            let count = e["count"].as_i64().filter(|c| *c > 1).map(|c| format!(" (×{c})")).unwrap_or_default();
            let line = format!(
                "{at} {} {}{count}: {}",
                e["type"].as_str().unwrap_or(""),
                e["reason"].as_str().unwrap_or(""),
                clip_line(e["message"].as_str().unwrap_or("").trim(), 600)
            );
            (at, line)
        })
        .collect();
    if events.is_empty() {
        return "(none; Kubernetes keeps events for about an hour)".into();
    }
    events.sort();
    let skip = events.len().saturating_sub(MAX_EVENTS);
    events.into_iter().skip(skip).map(|(_, l)| l).collect::<Vec<_>>().join("\n")
}

/// Containers worth reading the logs of: those that restarted, aren't ready or are waiting.
fn troubled_containers(pod: &Value) -> Vec<(String, i64)> {
    let statuses = pod["status"]["initContainerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(pod["status"]["containerStatuses"].as_array().into_iter().flatten());
    statuses
        .filter(|c| {
            c["restartCount"].as_i64().unwrap_or(0) > 0
                || c["ready"] == false && c["state"]["terminated"]["reason"] != "Completed"
                || c["state"]["waiting"].is_object()
        })
        .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["restartCount"].as_i64().unwrap_or(0))))
        .collect()
}

pub(crate) async fn read_log(api: &Api<Pod>, pod: &str, container: &str, previous: bool) -> String {
    read_log_lines(api, pod, Some(container), previous, LOG_LINES).await
}

pub(crate) async fn read_log_lines(api: &Api<Pod>, pod: &str, container: Option<&str>, previous: bool, lines: i64) -> String {
    let lp = LogParams { container: container.map(String::from), previous, tail_lines: Some(lines), limit_bytes: Some(64_000), ..Default::default() };
    match api.logs(pod, &lp).await {
        Ok(text) if text.trim().is_empty() => "(empty)".into(),
        Ok(text) => text.lines().map(|l| clip_line(l, 400)).collect::<Vec<_>>().join("\n"),
        Err(e) => format!("(not available: {})", crate::error::Error::from(e)),
    }
}

async fn pod_logs(client: &Client, ns: &str, name: &str, pod: &Value, max: usize, out: &mut String) {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let mut containers = troubled_containers(pod);
    if containers.is_empty() {
        // A healthy pod: its main container's latest lines still tell what it is doing.
        if let Some(c) = pod["spec"]["containers"][0]["name"].as_str() {
            containers.push((c.to_string(), 0));
        }
    }
    for (c, restarts) in containers.into_iter().take(max) {
        section(out, &format!("Last {LOG_LINES} log lines of container `{c}` in pod `{name}`"), &read_log(&api, name, &c, false).await, "");
        if restarts > 0 {
            section(out, &format!("Last {LOG_LINES} log lines of container `{c}` before its last restart"), &read_log(&api, name, &c, true).await, "");
        }
    }
}

/// The label selector of the pods a resource manages or routes to.
fn selector(kind: &KindRef, v: &Value) -> Option<String> {
    let map = if kind.group.is_empty() && kind.kind == "Service" { v["spec"]["selector"].as_object() } else { v["spec"]["selector"]["matchLabels"].as_object() }?;
    let parts: Vec<String> = map.iter().filter_map(|(k, v)| Some(format!("{k}={}", v.as_str()?))).collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

/// A line per pod, then the logs of the first pod in trouble.
async fn managed_pods(client: &Client, ns: &str, sel: &str, out: &mut String) {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let pods = match api.list(&ListParams::default().labels(sel).limit(30)).await {
        Ok(l) => l.items,
        Err(e) => {
            section(out, &format!("Pods matching {sel}"), &format!("(could not be read: {})", crate::error::Error::from(e)), "");
            return;
        }
    };
    if pods.is_empty() {
        section(out, &format!("Pods matching {sel}"), "(none)", "");
        return;
    }
    let pods: Vec<Value> = pods.into_iter().filter_map(|p| serde_json::to_value(p).ok()).collect();
    let lines: Vec<String> = pods.iter().map(pod_line).collect();
    section(out, &format!("Pods matching {sel}"), &lines.join("\n"), "");
    let troubled = pods.iter().find(|p| {
        let phase = p["status"]["phase"].as_str().unwrap_or("");
        phase == "Pending" || phase == "Failed" || !troubled_containers(p).is_empty()
    });
    if let Some(p) = troubled
        && let Some(name) = p["metadata"]["name"].as_str()
    {
        pod_logs(client, ns, name, p, 1, out).await;
    }
}

pub(crate) fn pod_line(p: &Value) -> String {
    let statuses: Vec<&Value> = p["status"]["containerStatuses"].as_array().into_iter().flatten().collect();
    let ready = statuses.iter().filter(|c| c["ready"] == true).count();
    let restarts: i64 = statuses.iter().filter_map(|c| c["restartCount"].as_i64()).sum();
    let mut line = format!(
        "{}: {}, ready {ready}/{}, restarts {restarts}",
        p["metadata"]["name"].as_str().unwrap_or("?"),
        p["status"]["phase"].as_str().unwrap_or("?"),
        statuses.len()
    );
    for c in &statuses {
        if let Some(r) = c["state"]["waiting"]["reason"].as_str() {
            line.push_str(&format!(", {} waiting: {r}", c["name"].as_str().unwrap_or("")));
        }
        if let Some(r) = c["lastState"]["terminated"]["reason"].as_str() {
            line.push_str(&format!(", {} last exit: {r} ({})", c["name"].as_str().unwrap_or(""), c["lastState"]["terminated"]["exitCode"]));
        }
    }
    if let Some(m) = p["status"]["conditions"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["type"] == "PodScheduled" && c["status"] == "False"))
        .and_then(|c| c["message"].as_str())
    {
        line.push_str(&format!(", not scheduled: {}", clip_line(m, 300)));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_in_text() {
        let s = redact("connecting to postgres://app:hunter2@db:5432/x with Authorization: Bearer abcdefghijklmnop and password=s3cr3t, token: \"xyz123\"");
        assert!(!s.contains("hunter2"), "{s}");
        assert!(!s.contains("abcdefghijklmnop"), "{s}");
        assert!(!s.contains("s3cr3t"), "{s}");
        assert!(!s.contains("xyz123"), "{s}");
        assert!(s.contains("postgres://app:[redacted]@db:5432/x"), "{s}");
    }

    #[test]
    fn keeps_harmless_fields() {
        let yaml = "automountServiceAccountToken: true\nsecretName: tls-cert\nsecret:\n  secretName: x\n";
        assert_eq!(redact(yaml), yaml);
    }

    #[test]
    fn redacts_sensitive_env_and_drops_noise() {
        let mut v = json!({
            "metadata": { "name": "p", "managedFields": [{}], "annotations": { "kubectl.kubernetes.io/last-applied-configuration": "{}", "team": "a" } },
            "spec": { "containers": [{ "name": "c", "env": [
                { "name": "DB_PASSWORD", "value": "hunter2" },
                { "name": "LOG_LEVEL", "value": "debug" },
                { "name": "API_TOKEN", "valueFrom": { "secretKeyRef": { "name": "s", "key": "k" } } }
            ] }] },
            "status": { "images": [1, 2] }
        });
        sanitize(&mut v);
        let env = &v["spec"]["containers"][0]["env"];
        assert_eq!(env[0]["value"], "[redacted]");
        assert_eq!(env[1]["value"], "debug");
        assert!(env[2].get("value").is_none());
        assert!(v["metadata"].get("managedFields").is_none());
        assert_eq!(v["metadata"]["annotations"], json!({ "team": "a" }));
        assert!(v["status"].get("images").is_none());
    }

    #[test]
    fn selectors() {
        let svc = KindRef { group: "".into(), version: "v1".into(), kind: "Service".into(), plural: "services".into(), namespaced: true, verbs: vec![] };
        let dep = KindRef { group: "apps".into(), version: "v1".into(), kind: "Deployment".into(), plural: "deployments".into(), namespaced: true, verbs: vec![] };
        assert_eq!(selector(&svc, &json!({ "spec": { "selector": { "app": "web" } } })).as_deref(), Some("app=web"));
        assert_eq!(selector(&dep, &json!({ "spec": { "selector": { "matchLabels": { "app": "web", "tier": "fe" } } } })).as_deref(), Some("app=web,tier=fe"));
        assert_eq!(selector(&dep, &json!({ "spec": {} })), None);
    }

    #[test]
    fn troubled() {
        let pod = json!({ "status": { "containerStatuses": [
            { "name": "ok", "ready": true, "restartCount": 0, "state": { "running": {} } },
            { "name": "crash", "ready": false, "restartCount": 4, "state": { "waiting": { "reason": "CrashLoopBackOff" } } },
            { "name": "done", "ready": false, "restartCount": 0, "state": { "terminated": { "reason": "Completed" } } }
        ] } });
        assert_eq!(troubled_containers(&pod), vec![("crash".to_string(), 4)]);
    }

    /// Writes, reads and deletes a test entry in the real keychain: `cargo test --lib keychain -- --ignored`
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let e = keychain("roundtrip-test").expect("entry");
        e.set_password("value-1").expect("set");
        assert_eq!(e.get_password().expect("get"), "value-1");
        e.delete_credential().expect("delete");
        assert!(e.get_password().is_err());
    }

    #[test]
    fn default_addresses() {
        let ai = AiSettings { provider: "ollama".into(), ..Default::default() };
        assert_eq!(ai.base().unwrap(), "http://localhost:11434/v1");
        let ai = AiSettings { provider: "custom".into(), base_url: "http://gpu-box:8000/v1/".into(), ..Default::default() };
        assert_eq!(ai.base().unwrap(), "http://gpu-box:8000/v1");
        assert!(AiSettings { provider: "custom".into(), ..Default::default() }.base().is_err());
    }
}
