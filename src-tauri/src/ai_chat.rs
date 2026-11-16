//! "Ask AI": a conversation in which the model can read the clusters with read-only tools
//! (clusters, resources, YAML, events, logs). It never changes anything: when a change is needed it
//! shows the command for the user to run.
//!
//! The conversation is kept here, in memory, in a neutral form, and converted to the provider's
//! format on every request (OpenAI-compatible chat completions or Anthropic messages).

use std::collections::HashMap;
use std::sync::LazyLock;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::api::{Api, ListParams};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::ai::{self, AiSettings};
use crate::commands::{dyn_api, kinds_for};
use crate::error::{Error, Result};
use crate::state::{AppState, KindRef, StreamHandle};

/// Model calls per question: enough to look at a few things, then answer.
const MAX_STEPS: usize = 10;
/// What one tool can return to the model.
const MAX_RESULT: usize = 14_000;
const MAX_ROWS: usize = 200;

#[derive(Debug, Clone)]
struct Call {
    id: String,
    name: String,
    args: Value,
}

#[derive(Debug, Clone)]
enum Msg {
    User(String),
    Assistant { text: String, calls: Vec<Call> },
    Tool { id: String, content: String },
}

static CHATS: LazyLock<Mutex<HashMap<u64, Vec<Msg>>>> = LazyLock::new(Default::default);

/// A cluster the model may read, with its environment as the UI knows it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContext {
    pub name: String,
    pub env: Option<String>,
}

/// A conversation that starts from an "Explain" answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seed {
    pub question: String,
    pub answer: String,
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatMsg {
    Delta { text: String },
    /// The model reads something; `label` says what, in words.
    ToolStart { id: String, label: String },
    ToolEnd { id: String, ok: bool, summary: String },
    /// A change the model suggests; the user applies it (or not) with the usual confirmations.
    Proposal { proposal: Box<Proposal> },
    Done,
    Error { message: String },
}

const SYSTEM: &str = "You are the AI assistant built into Steerglass, a desktop app for operating many Kubernetes clusters at \
once. You help the user understand what is happening in their clusters.

You have read-only tools. Use them to look at real data instead of guessing, and call several when needed: for example list \
the problems first, then read the events and logs of what fails, or read the same deployment in two clusters to compare them. \
Don't ask the user for data you can read yourself. Prefer narrow calls (a namespace, a label selector, only problems).

You cannot change anything yourself. To suggest a change the app can make (scale, rollout restart, cordon, uncordon, suspend or \
resume a CronJob, or apply YAML), call propose_change: the user sees it with an Apply button, reviews it and confirms it; you \
won't know whether they did. Read the object first, propose one change per call, and for apply give a minimal manifest with \
only the fields to set (it is a server-side apply). For other changes, show the kubectl command for the user to run, and say \
which cluster and namespace it is for. Warn clearly when a cluster is production. Never suggest deleting data, volumes or \
namespaces as a fix. Secrets are never available to you, and values that look like passwords or tokens are redacted.

Answer in Markdown, short and concrete: lead with the answer, then the evidence (quote the event or log line), then next steps. \
No preamble.";

#[tauri::command]
pub fn ai_chat_reset(chat: u64) {
    CHATS.lock().remove(&chat);
}

/// Sends a question in conversation `chat` (created on first use, from `seed` if given) and
/// streams the answer, with the tools the model uses. Stop it with `stream_stop`.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn ai_chat_send(
    app: AppHandle,
    state: State<'_, AppState>,
    chat: u64,
    text: String,
    contexts: Vec<ChatContext>,
    hidden_production: usize,
    seed: Option<Seed>,
    on_event: Channel<ChatMsg>,
) -> Result<u64> {
    let ai = state.settings.read().ai.clone();
    if !ai.enabled {
        return Err(Error::msg("AI is off. Turn it on in Settings → AI."));
    }
    if ai.model.trim().is_empty() {
        return Err(Error::msg("Choose a model in Settings → AI."));
    }
    let key = ai::key_for(&state, &ai)?;
    let mut messages = CHATS.lock().get(&chat).cloned().unwrap_or_else(|| match seed {
        Some(s) => vec![Msg::User(s.question), Msg::Assistant { text: s.answer, calls: vec![] }],
        None => vec![],
    });
    messages.push(Msg::User(text));
    CHATS.lock().insert(chat, messages.clone());

    let system = ai.system(&format!("{SYSTEM}\n\n{}", clusters_note(&contexts, hidden_production)));
    let id = state.next_id();
    let app2 = app.clone();
    let task = tokio::spawn(async move {
        let st = tauri::Manager::state::<AppState>(&app2);
        let end = match converse(&st, &ai, key.as_deref(), &system, chat, messages, &contexts, &on_event).await {
            Ok(()) => ChatMsg::Done,
            Err(e) => ChatMsg::Error { message: e.to_string() },
        };
        let _ = on_event.send(end);
        crate::streams::unregister(&app2, id);
    });
    crate::streams::register(&app, id, StreamHandle { abort: task.abort_handle(), stdin: None, resize: None, forward: None });
    Ok(id)
}

fn clusters_note(contexts: &[ChatContext], hidden_production: usize) -> String {
    let mut s = format!("Current time: {}.\nClusters you can read (use these exact names):\n", ai::now());
    for c in contexts {
        s.push_str(&format!("- {}{}\n", c.name, c.env.as_deref().map(|e| format!(" ({e})")).unwrap_or_default()));
    }
    if contexts.is_empty() {
        s.push_str("(none)\n");
    }
    if hidden_production > 0 {
        s.push_str(&format!(
            "{hidden_production} production cluster(s) are not available to you: the user hasn't allowed AI on production. If the question is about them, say so.\n"
        ));
    }
    s
}

#[allow(clippy::too_many_arguments)]
async fn converse(
    state: &AppState,
    ai: &AiSettings,
    key: Option<&str>,
    system: &str,
    chat: u64,
    mut messages: Vec<Msg>,
    contexts: &[ChatContext],
    on: &Channel<ChatMsg>,
) -> Result<()> {
    for step in 0..MAX_STEPS {
        // Last step: no tools, so the model has to answer with what it has.
        let tools = step + 1 < MAX_STEPS;
        let (text, calls) = model_turn(ai, key, system, &messages, tools, on).await?;
        let done = calls.is_empty();
        messages.push(Msg::Assistant { text, calls: calls.clone() });
        for c in calls {
            if c.name == "propose_change" {
                let content = match proposal(state, contexts, &c).await {
                    Ok(p) => {
                        let _ = on.send(ChatMsg::Proposal { proposal: Box::new(p) });
                        "Shown to the user with an Apply button. They review and confirm it themselves; you don't know whether they applied it.".to_string()
                    }
                    Err(e) => format!("Error: {e}. Nothing was shown to the user."),
                };
                messages.push(Msg::Tool { id: c.id, content });
                continue;
            }
            let label = tool_label(&c);
            let _ = on.send(ChatMsg::ToolStart { id: c.id.clone(), label });
            let (ok, content, summary) = match run_tool(state, contexts, &c).await {
                Ok((content, summary)) => (true, content, summary),
                Err(e) => (false, format!("Error: {e}"), e.to_string()),
            };
            let _ = on.send(ChatMsg::ToolEnd { id: c.id.clone(), ok, summary });
            messages.push(Msg::Tool { id: c.id, content: ai::redact(&ai::clip(&content, MAX_RESULT)) });
        }
        // Saved only at consistent points (every tool call has its result), so a stopped answer
        // leaves a conversation that can go on.
        if let Some(saved) = CHATS.lock().get_mut(&chat) {
            *saved = messages.clone();
        } else {
            return Ok(()); // the conversation was reset meanwhile
        }
        if done {
            return Ok(());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- tools

fn tools_spec() -> Vec<(&'static str, &'static str, Value)> {
    let cluster = json!({ "type": "string", "description": "Cluster name, exactly as listed" });
    let kind = json!({ "type": "string", "description": "Resource kind: Pod, Deployment, StatefulSet, DaemonSet, Job, CronJob, Service, Ingress, Node, Namespace, ConfigMap, PersistentVolumeClaim, HorizontalPodAutoscaler, or any CRD kind" });
    let namespace = json!({ "type": "string", "description": "Namespace; omit for all namespaces or cluster-scoped kinds" });
    vec![
        (
            "list_resources",
            "List resources of a kind in a cluster, one line each with kubectl-style columns and a health mark (OK, WARN, ERROR). Use only_problems to see just the unhealthy ones.",
            json!({ "type": "object", "properties": {
                "cluster": cluster, "kind": kind, "namespace": namespace,
                "label_selector": { "type": "string", "description": "e.g. app=web,tier=frontend" },
                "only_problems": { "type": "boolean" }
            }, "required": ["cluster", "kind"] }),
        ),
        (
            "get_resource",
            "Read one resource as YAML (spec and status; sensitive values redacted). For workloads and services, also lists the pods they select. Secrets can't be read.",
            json!({ "type": "object", "properties": {
                "cluster": cluster, "kind": kind, "namespace": namespace, "name": { "type": "string" }
            }, "required": ["cluster", "kind", "name"] }),
        ),
        (
            "get_events",
            "Recent Kubernetes events (kept about an hour), newest last. Give kind and name for one object's events, or a namespace (or nothing) for all; only_warnings keeps the warnings.",
            json!({ "type": "object", "properties": {
                "cluster": cluster, "namespace": namespace, "kind": kind, "name": { "type": "string" },
                "only_warnings": { "type": "boolean" }
            }, "required": ["cluster"] }),
        ),
        (
            "get_logs",
            "The last lines of a pod's container logs. previous=true reads the container's logs from before its last restart (why it crashed).",
            json!({ "type": "object", "properties": {
                "cluster": cluster, "namespace": { "type": "string" }, "pod": { "type": "string" },
                "container": { "type": "string", "description": "Needed when the pod has several containers" },
                "previous": { "type": "boolean" },
                "lines": { "type": "integer", "description": "How many lines, up to 300 (default 100)" }
            }, "required": ["cluster", "namespace", "pod"] }),
        ),
        (
            "propose_change",
            "Suggest one change for the user to apply with a button (they confirm it; nothing happens otherwise). Actions: scale (Deployment, StatefulSet, ReplicaSet; needs replicas), restart (rollout restart of a Deployment, StatefulSet or DaemonSet), cordon / uncordon (Node), suspend / resume (CronJob), apply (a YAML manifest applied with server-side apply: give only the fields to set, with apiVersion, kind, metadata.name and metadata.namespace).",
            json!({ "type": "object", "properties": {
                "cluster": cluster,
                "action": { "type": "string", "enum": ["scale", "restart", "cordon", "uncordon", "suspend", "resume", "apply"] },
                "kind": kind, "namespace": { "type": "string" }, "name": { "type": "string" },
                "replicas": { "type": "integer" },
                "manifest": { "type": "string", "description": "YAML, for apply" },
                "reason": { "type": "string", "description": "One sentence: why this change" }
            }, "required": ["cluster", "action", "reason"] }),
        ),
    ]
}

/// A change suggested by the model, checked here and applied by the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub id: String,
    pub cluster: String,
    pub action: String,
    pub kind: Option<KindRef>,
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub replicas: Option<i64>,
    pub manifest: Option<String>,
    pub reason: String,
}

async fn proposal(state: &AppState, contexts: &[ChatContext], c: &Call) -> Result<Proposal> {
    let cluster = arg(c, "cluster").ok_or_else(|| Error::msg("cluster is required"))?;
    if !contexts.iter().any(|x| x.name == cluster) {
        return Err(Error::msg(format!("cluster `{cluster}` isn't available")));
    }
    let action = arg(c, "action").ok_or_else(|| Error::msg("action is required"))?.to_lowercase();
    let mut p = Proposal {
        id: c.id.clone(),
        cluster: cluster.into(),
        action: action.clone(),
        kind: None,
        namespace: arg(c, "namespace").map(String::from),
        name: arg(c, "name").map(String::from),
        replicas: None,
        manifest: None,
        reason: arg(c, "reason").unwrap_or("").to_string(),
    };
    if action == "apply" {
        let manifest = arg(c, "manifest").ok_or_else(|| Error::msg("manifest is required for apply"))?;
        let mut n = 0;
        for doc in serde_yaml::Deserializer::from_str(manifest) {
            let v = Value::deserialize(doc).map_err(|e| Error::msg(format!("invalid YAML: {e}")))?;
            if v.is_null() {
                continue;
            }
            n += 1;
            let kind = v["kind"].as_str().ok_or_else(|| Error::msg("every object needs kind"))?;
            if v["metadata"]["name"].as_str().is_none() || v["apiVersion"].as_str().is_none() {
                return Err(Error::msg("every object needs apiVersion and metadata.name"));
            }
            if kind == "Secret" {
                return Err(Error::msg("Secrets can't be proposed"));
            }
        }
        if n == 0 || n > 10 {
            return Err(Error::msg("the manifest must have between 1 and 10 objects"));
        }
        p.manifest = Some(manifest.to_string());
        return Ok(p);
    }
    let kind = resolve_kind(state, cluster, arg(c, "kind").ok_or_else(|| Error::msg("kind is required"))?).await?;
    if p.name.is_none() {
        return Err(Error::msg("name is required"));
    }
    let allowed: &[&str] = match action.as_str() {
        "scale" => &["Deployment", "StatefulSet", "ReplicaSet"],
        "restart" => &["Deployment", "StatefulSet", "DaemonSet"],
        "cordon" | "uncordon" => &["Node"],
        "suspend" | "resume" => &["CronJob"],
        other => return Err(Error::msg(format!("unknown action {other}"))),
    };
    if !allowed.contains(&kind.kind.as_str()) {
        return Err(Error::msg(format!("{action} works on {}, not {}", allowed.join(", "), kind.kind)));
    }
    if kind.namespaced && p.namespace.is_none() {
        return Err(Error::msg("namespace is required"));
    }
    if action == "scale" {
        let r = c.args.get("replicas").and_then(Value::as_i64).filter(|r| (0..=1000).contains(r));
        p.replicas = Some(r.ok_or_else(|| Error::msg("replicas (0-1000) is required for scale"))?);
    }
    p.kind = Some(kind);
    Ok(p)
}

fn arg<'a>(c: &'a Call, name: &str) -> Option<&'a str> {
    c.args.get(name).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn flag(c: &Call, name: &str) -> bool {
    c.args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn tool_label(c: &Call) -> String {
    let cluster = arg(c, "cluster").unwrap_or("?");
    let ns = arg(c, "namespace").map(|n| format!(" / {n}")).unwrap_or_default();
    match c.name.as_str() {
        "list_resources" => format!(
            "Listed {}{} in {cluster}{ns}",
            if flag(c, "only_problems") { "failing " } else { "" },
            arg(c, "kind").unwrap_or("resources")
        ),
        "get_resource" => format!("Read {} {} in {cluster}{ns}", arg(c, "kind").unwrap_or(""), arg(c, "name").unwrap_or("")),
        "get_events" => match (arg(c, "kind"), arg(c, "name")) {
            (Some(k), Some(n)) => format!("Read the events of {k} {n} in {cluster}"),
            _ => format!("Read the {}events in {cluster}{ns}", if flag(c, "only_warnings") { "warning " } else { "" }),
        },
        "get_logs" => format!(
            "Read the {}logs of {} in {cluster}{ns}",
            if flag(c, "previous") { "previous " } else { "" },
            arg(c, "pod").unwrap_or("")
        ),
        other => format!("Used {other}"),
    }
}

/// Runs a tool: (what the model gets, a short summary for the user).
async fn run_tool(state: &AppState, contexts: &[ChatContext], c: &Call) -> Result<(String, String)> {
    let cluster = arg(c, "cluster").ok_or_else(|| Error::msg("cluster is required"))?;
    if !contexts.iter().any(|x| x.name == cluster) {
        return Err(Error::msg(format!("cluster `{cluster}` isn't available (it doesn't exist, or AI isn't allowed on it)")));
    }
    let ns = arg(c, "namespace");
    match c.name.as_str() {
        "list_resources" => {
            let kind = resolve_kind(state, cluster, arg(c, "kind").ok_or_else(|| Error::msg("kind is required"))?).await?;
            let client = state.client(cluster).await?;
            let url = dyn_api(client.clone(), &kind, ns).resource_url().to_string();
            let rows = crate::table::list_rows_labeled(&client, &url, &kind.group, &kind.kind, arg(c, "label_selector"))
                .await
                .map_err(Error::Msg)?;
            let total = rows.len();
            let only_problems = flag(c, "only_problems");
            let rows: Vec<_> = rows.into_iter().filter(|r| !only_problems || r.health != crate::rows::Health::Ok).collect();
            let cols = crate::rows::columns(&kind.group, &kind.kind);
            let mut out = format!(
                "{} {}{} in {cluster}{}: {}{}\nColumns: {}name | {} | health\n",
                rows.len(),
                if only_problems { "unhealthy " } else { "" },
                kind.kind,
                ns.map(|n| format!(" / {n}")).unwrap_or_default(),
                if only_problems { format!("{total} in all") } else { String::new() },
                if rows.len() > MAX_ROWS { format!(", first {MAX_ROWS} shown") } else { String::new() },
                if kind.namespaced && ns.is_none() { "namespace | " } else { "" },
                cols.join(" | "),
            );
            for r in rows.iter().take(MAX_ROWS) {
                let health = match r.health {
                    crate::rows::Health::Ok => "OK",
                    crate::rows::Health::Warn => "WARN",
                    crate::rows::Health::Error => "ERROR",
                    crate::rows::Health::Neutral => "-",
                };
                let nsc = if kind.namespaced && ns.is_none() { format!("{} | ", r.namespace.as_deref().unwrap_or("")) } else { String::new() };
                out.push_str(&format!("{nsc}{} | {} | {health}\n", r.name, r.cells.iter().map(|s| ai::clip_line(s, 120)).collect::<Vec<_>>().join(" | ")));
            }
            Ok((out, format!("{} row{}", rows.len(), if rows.len() == 1 { "" } else { "s" })))
        }
        "get_resource" => {
            let kind = resolve_kind(state, cluster, arg(c, "kind").ok_or_else(|| Error::msg("kind is required"))?).await?;
            if kind.group.is_empty() && kind.kind == "Secret" {
                return Err(Error::msg("Secrets are never available to the AI"));
            }
            let name = arg(c, "name").ok_or_else(|| Error::msg("name is required"))?;
            let client = state.client(cluster).await?;
            let obj = dyn_api(client.clone(), &kind, ns).get(name).await?;
            let mut v = serde_json::to_value(&obj)?;
            ai::sanitize(&mut v);
            let mut out = ai::clip(&serde_yaml::to_string(&v)?, 11_000);
            let sel = selector_of(&kind, &v);
            if let (Some(n), Some(sel)) = (ns, sel) {
                let pods: Api<Pod> = Api::namespaced(client, n);
                if let Ok(list) = pods.list(&ListParams::default().labels(&sel).limit(30)).await {
                    out.push_str(&format!("\nPods selected by {sel}:\n"));
                    for p in list.items.into_iter().filter_map(|p| serde_json::to_value(p).ok()) {
                        out.push_str(&format!("- {}\n", ai::pod_line(&p)));
                    }
                }
            }
            Ok((out, "read".into()))
        }
        "get_events" => {
            let client = state.client(cluster).await?;
            if let (Some(k), Some(n)) = (arg(c, "kind"), arg(c, "name")) {
                let kind = resolve_kind(state, cluster, k).await.map(|k| k.kind).unwrap_or_else(|_| k.to_string());
                let text = ai::events_for(&client, &kind, ns, n).await;
                let count = text.lines().filter(|l| !l.starts_with('(')).count();
                return Ok((text, format!("{count} event{}", if count == 1 { "" } else { "s" })));
            }
            let api: Api<Event> = match ns {
                Some(n) => Api::namespaced(client, n),
                None => Api::all(client),
            };
            let mut lp = ListParams::default();
            if flag(c, "only_warnings") {
                lp = lp.fields("type=Warning");
            }
            let list = api.list(&lp).await?;
            let mut events: Vec<(String, String)> = list
                .items
                .into_iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .map(|e| {
                    let at = e["lastTimestamp"].as_str().or(e["eventTime"].as_str()).or(e["metadata"]["creationTimestamp"].as_str()).unwrap_or("").to_string();
                    let count = e["count"].as_i64().filter(|c| *c > 1).map(|c| format!(" (×{c})")).unwrap_or_default();
                    let obj = &e["involvedObject"];
                    let line = format!(
                        "{at} {} {} {}/{}{}{count}: {}",
                        e["type"].as_str().unwrap_or(""),
                        e["reason"].as_str().unwrap_or(""),
                        obj["kind"].as_str().unwrap_or(""),
                        obj["name"].as_str().unwrap_or(""),
                        obj["namespace"].as_str().map(|n| format!(" in {n}")).unwrap_or_default(),
                        ai::clip_line(e["message"].as_str().unwrap_or("").trim(), 400)
                    );
                    (at, line)
                })
                .collect();
            events.sort();
            let n = events.len();
            let skip = n.saturating_sub(80);
            let text = if n == 0 {
                "(no events; Kubernetes keeps them for about an hour)".to_string()
            } else {
                format!("{n} events{}:\n{}", if skip > 0 { ", the latest 80" } else { "" }, events.into_iter().skip(skip).map(|(_, l)| l).collect::<Vec<_>>().join("\n"))
            };
            Ok((text, format!("{n} event{}", if n == 1 { "" } else { "s" })))
        }
        "get_logs" => {
            let ns = ns.ok_or_else(|| Error::msg("namespace is required"))?;
            let pod = arg(c, "pod").ok_or_else(|| Error::msg("pod is required"))?;
            let lines = c.args.get("lines").and_then(Value::as_i64).unwrap_or(100).clamp(1, 300);
            let api: Api<Pod> = Api::namespaced(state.client(cluster).await?, ns);
            let text = ai::read_log_lines(&api, pod, arg(c, "container"), flag(c, "previous"), lines).await;
            let n = if text.starts_with('(') { 0 } else { text.lines().count() };
            Ok((text, format!("{n} line{}", if n == 1 { "" } else { "s" })))
        }
        other => Err(Error::msg(format!("unknown tool {other}"))),
    }
}

fn selector_of(kind: &KindRef, v: &Value) -> Option<String> {
    let map = if kind.group.is_empty() && kind.kind == "Service" { v["spec"]["selector"].as_object() } else { v["spec"]["selector"]["matchLabels"].as_object() }?;
    let parts: Vec<String> = map.iter().filter_map(|(k, v)| Some(format!("{k}={}", v.as_str()?))).collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

const ALIASES: &[(&str, &str)] = &[
    ("po", "pods"),
    ("deploy", "deployments"),
    ("sts", "statefulsets"),
    ("ds", "daemonsets"),
    ("rs", "replicasets"),
    ("cj", "cronjobs"),
    ("svc", "services"),
    ("ing", "ingresses"),
    ("ns", "namespaces"),
    ("no", "nodes"),
    ("cm", "configmaps"),
    ("pvc", "persistentvolumeclaims"),
    ("pv", "persistentvolumes"),
    ("hpa", "horizontalpodautoscalers"),
    ("ev", "events"),
    ("sa", "serviceaccounts"),
    ("ep", "endpoints"),
    ("pdb", "poddisruptionbudgets"),
    ("netpol", "networkpolicies"),
];

/// Finds a kind by name, plural or kubectl short name ("Deployment", "deployments", "deploy",
/// "deployments.apps"), preferring the core groups when several match.
async fn resolve_kind(state: &AppState, cluster: &str, name: &str) -> Result<KindRef> {
    let kinds = kinds_for(state, cluster, false).await?;
    let lower = name.to_lowercase();
    let (lower, group) = match lower.split_once('.') {
        Some((n, g)) => (n.to_string(), Some(g.to_string())),
        None => (lower, None),
    };
    let wanted = ALIASES.iter().find(|(a, _)| *a == lower).map(|(_, p)| p.to_string()).unwrap_or(lower);
    let mut found: Vec<&KindRef> = kinds
        .iter()
        .filter(|k| {
            let kind = k.kind.to_lowercase();
            (kind == wanted || k.plural == wanted || format!("{kind}s") == wanted) && group.as_ref().is_none_or(|g| k.group.starts_with(g.as_str()))
        })
        .collect();
    found.sort_by_key(|k| (!k.group.is_empty(), k.group.len()));
    found.first().map(|k| (*k).clone()).ok_or_else(|| Error::msg(format!("unknown kind `{name}` in {cluster}")))
}

// ---------------------------------------------------------------- providers

fn openai_messages(system: &str, messages: &[Msg]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        out.push(match m {
            Msg::User(t) => json!({ "role": "user", "content": t }),
            Msg::Assistant { text, calls } if calls.is_empty() => json!({ "role": "assistant", "content": text }),
            Msg::Assistant { text, calls } => json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { json!(text) },
                "tool_calls": calls.iter().map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.args.to_string() } })).collect::<Vec<_>>(),
            }),
            Msg::Tool { id, content } => json!({ "role": "tool", "tool_call_id": id, "content": content }),
        });
    }
    out
}

/// Anthropic wants alternating turns: consecutive user content (tool results, then a question) is merged.
fn anthropic_messages(messages: &[Msg]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, block: Value| {
        if let Some(last) = out.last_mut().filter(|l| l["role"] == role) {
            last["content"].as_array_mut().unwrap().push(block);
        } else {
            out.push(json!({ "role": role, "content": [block] }));
        }
    };
    for m in messages {
        match m {
            Msg::User(t) => push("user", json!({ "type": "text", "text": t })),
            Msg::Assistant { text, calls } => {
                if !text.is_empty() {
                    push("assistant", json!({ "type": "text", "text": text }));
                }
                for c in calls {
                    push("assistant", json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": c.args }));
                }
                if text.is_empty() && calls.is_empty() {
                    push("assistant", json!({ "type": "text", "text": "(no answer)" }));
                }
            }
            Msg::Tool { id, content } => push("user", json!({ "type": "tool_result", "tool_use_id": id, "content": content })),
        }
    }
    out
}

/// One model call: streams its text and returns it with the tool calls it asked for.
async fn model_turn(ai: &AiSettings, key: Option<&str>, system: &str, messages: &[Msg], tools: bool, on: &Channel<ChatMsg>) -> Result<(String, Vec<Call>)> {
    let anthropic = ai.provider == "anthropic";
    let specs = tools_spec();
    let mut body = if anthropic {
        json!({ "model": ai.model, "max_tokens": 4096, "stream": true, "system": system, "messages": anthropic_messages(messages) })
    } else {
        json!({ "model": ai.model, "stream": true, "messages": openai_messages(system, messages) })
    };
    if tools {
        body["tools"] = if anthropic {
            json!(specs.iter().map(|(n, d, p)| json!({ "name": n, "description": d, "input_schema": p })).collect::<Vec<_>>())
        } else {
            json!(specs.iter().map(|(n, d, p)| json!({ "type": "function", "function": { "name": n, "description": d, "parameters": p } })).collect::<Vec<_>>())
        };
    }
    let path = if anthropic { "/messages" } else { "/chat/completions" };
    let (base, req) = ai::request(ai, key, reqwest::Method::POST, path)?;
    let resp = req.json(&body).send().await.map_err(|e| ai::net_error(ai, &base, e))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let err = ai::api_error(status, &body);
        if status.as_u16() == 400 && body.to_lowercase().contains("tool") {
            return Err(Error::msg(format!("{err} This model may not support tools: pick another one in Settings → AI.")));
        }
        return Err(err);
    }

    let mut text = String::new();
    // Tool calls by stream index: (id, name, arguments as streamed).
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let is_json = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        let v: Value = resp.json().await.map_err(|e| ai::net_error(ai, &base, e))?;
        if anthropic {
            for b in v["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                    Some("tool_use") => calls.push((b["id"].as_str().unwrap_or("").into(), b["name"].as_str().unwrap_or("").into(), b["input"].to_string())),
                    _ => {}
                }
            }
        } else {
            let m = &v["choices"][0]["message"];
            text.push_str(m["content"].as_str().unwrap_or(""));
            for c in m["tool_calls"].as_array().into_iter().flatten() {
                calls.push((c["id"].as_str().unwrap_or("").into(), c["function"]["name"].as_str().unwrap_or("").into(), c["function"]["arguments"].as_str().unwrap_or("{}").into()));
            }
        }
        if !text.is_empty() {
            let _ = on.send(ChatMsg::Delta { text: text.clone() });
        }
        return Ok((text, finish_calls(calls)));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    // Anthropic: content block index → position in `calls`.
    let mut blocks: HashMap<u64, usize> = HashMap::new();
    'read: while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| ai::net_error(ai, &base, e))?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break 'read;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(msg) = v["error"]["message"].as_str() {
                return Err(Error::msg(msg.to_string()));
            }
            let mut delta = None;
            if anthropic {
                match v["type"].as_str() {
                    Some("content_block_start") if v["content_block"]["type"] == "tool_use" => {
                        let b = &v["content_block"];
                        blocks.insert(v["index"].as_u64().unwrap_or(0), calls.len());
                        calls.push((b["id"].as_str().unwrap_or("").into(), b["name"].as_str().unwrap_or("").into(), String::new()));
                    }
                    Some("content_block_delta") => {
                        if let Some(t) = v["delta"]["text"].as_str() {
                            delta = Some(t.to_string());
                        } else if let (Some(p), Some(i)) = (v["delta"]["partial_json"].as_str(), blocks.get(&v["index"].as_u64().unwrap_or(0))) {
                            calls[*i].2.push_str(p);
                        }
                    }
                    Some("message_stop") => break 'read,
                    _ => {}
                }
            } else {
                let d = &v["choices"][0]["delta"];
                if let Some(t) = d["content"].as_str() {
                    delta = Some(t.to_string());
                }
                for tc in d["tool_calls"].as_array().into_iter().flatten() {
                    let i = tc["index"].as_u64().map(|i| i as usize).unwrap_or(calls.len());
                    while calls.len() <= i {
                        calls.push(Default::default());
                    }
                    let c = &mut calls[i];
                    if let Some(id) = tc["id"].as_str() {
                        c.0 = id.into();
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        c.1.push_str(n);
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        c.2.push_str(a);
                    }
                }
            }
            if let Some(t) = delta.filter(|t| !t.is_empty()) {
                text.push_str(&t);
                if on.send(ChatMsg::Delta { text: t }).is_err() {
                    break 'read;
                }
            }
        }
    }
    Ok((text, finish_calls(calls)))
}

fn finish_calls(raw: Vec<(String, String, String)>) -> Vec<Call> {
    raw.into_iter()
        .enumerate()
        .filter(|(_, (_, name, _))| !name.is_empty())
        .map(|(i, (id, name, args))| Call {
            id: if id.is_empty() { format!("call_{i}") } else { id },
            name,
            args: serde_json::from_str(if args.trim().is_empty() { "{}" } else { &args }).unwrap_or_else(|_| json!({})),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo() -> Vec<Msg> {
        vec![
            Msg::User("why?".into()),
            Msg::Assistant { text: String::new(), calls: vec![Call { id: "t1".into(), name: "get_logs".into(), args: json!({ "pod": "p" }) }] },
            Msg::Tool { id: "t1".into(), content: "FATAL".into() },
            Msg::User("and now?".into()),
        ]
    }

    #[test]
    fn anthropic_turns_alternate() {
        let m = anthropic_messages(&convo());
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(m[1]["content"][0]["type"], "tool_use");
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][1]["text"], "and now?");
    }

    #[test]
    fn openai_tool_calls() {
        let m = openai_messages("sys", &convo());
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"pod\":\"p\"}");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "t1");
    }

    #[test]
    fn calls_from_stream_pieces() {
        let calls = finish_calls(vec![("".into(), "list_resources".into(), "{\"cluster\":\"a\",".to_string() + "\"kind\":\"pods\"}"), ("x".into(), "".into(), "".into())]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].args["kind"], "pods");
    }

    /// The tool loop against a fake server (scripts: a request with a tool call, then an answer):
    /// `MOCK_AI_URL=http://127.0.0.1:18081/v1 cargo test --lib tool_loop -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn tool_loop_against_mock() {
        let Ok(url) = std::env::var("MOCK_AI_URL") else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join("steerglass-chat-test");
        // No kubeconfig: the tool fails, and the error goes back to the model.
        unsafe {
            std::env::set_var("KUBECONFIG", dir.join("none"));
            std::env::set_var("STEERGLASS_KUBE_DIR", &dir);
        }
        let state = AppState::new(dir.join("settings.json"));
        for (chat, provider) in [(1u64, "custom"), (2, "anthropic")] {
            let ai = AiSettings { enabled: true, provider: provider.into(), base_url: url.clone(), model: "m".into(), ..Default::default() };
            let events = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
            let ev = events.clone();
            let ch: Channel<ChatMsg> = Channel::new(move |b| {
                ev.lock().push(format!("{b:?}"));
                Ok(())
            });
            let msgs = vec![Msg::User("what fails?".into())];
            CHATS.lock().insert(chat, msgs.clone());
            let ctx = [ChatContext { name: "demo".into(), env: None }];
            converse(&state, &ai, Some("k"), "sys", chat, msgs, &ctx, &ch).await.expect(provider);
            let saved = CHATS.lock().get(&chat).cloned().unwrap();
            assert_eq!(saved.len(), 4, "{provider}: {saved:?}");
            let Msg::Assistant { text, calls } = &saved[1] else { panic!("{saved:?}") };
            assert_eq!(text, "Let me look. ");
            assert_eq!(calls[0].name, "list_resources");
            assert_eq!(calls[0].args["only_problems"], true);
            let Msg::Tool { content, .. } = &saved[2] else { panic!() };
            assert!(content.starts_with("Error:"), "{content}");
            let Msg::Assistant { text, calls } = &saved[3] else { panic!() };
            assert!(text.starts_with("Done: Error:"), "{text}");
            assert!(calls.is_empty());
            println!("{provider}: {} events, tool result: {content}", events.lock().len());
        }
    }

    #[test]
    fn labels() {
        let c = Call { id: "1".into(), name: "get_logs".into(), args: json!({ "cluster": "prod-eu", "namespace": "shop", "pod": "web-1", "previous": true }) };
        assert_eq!(tool_label(&c), "Read the previous logs of web-1 in prod-eu / shop");
    }
}
