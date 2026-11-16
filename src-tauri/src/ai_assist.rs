//! Short AI helpers: ⌘K in plain words (navigation) and a shell command from a description.
//! Neither runs anything: navigation only moves the view, and the command is typed in the terminal
//! for the user to read, change and run with Enter.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::ai;
use crate::error::{Error, Result};
use crate::state::AppState;

// ---------------------------------------------------------------- ⌘K

const PALETTE_PROMPT: &str = r#"You turn a request typed in the command palette of Steerglass, a Kubernetes desktop app, into a JSON object. Reply with the JSON object only.

When the request is about what to look at, navigate:
{"action": "navigate", "clusters": [names], "kind": "Pod", "namespace": "payments", "filter": "status:bad", "open": "name"}
- clusters: one or more of the available cluster names, exactly as given. Omit to keep the current ones. Words like "staging" or "prod" mean every cluster whose name or environment matches.
- kind: a kind from the list (singular, as written there). Omit to keep the current one.
- namespace: a namespace name, "" for all namespaces, or omit to keep the current one.
- filter: words that must all appear in a row (name or any column), or "status:bad" for failing or warning rows only, "status:error", "status:warn", "status:ok". Omit for no filter.
- open: the exact name of one object to open, only when the request names one.

When it is a question that needs reading data and thinking (why, how, compare, explain, what changed, which ones use more…), answer:
{"action": "ask"}"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaletteState {
    pub clusters: Vec<ClusterName>,
    pub kinds: Vec<String>,
    pub namespaces: Vec<String>,
    pub current: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterName {
    pub name: String,
    pub env: Option<String>,
}

/// What to do with a ⌘K request: `{"action": "navigate", …}` or `{"action": "ask"}`.
#[tauri::command]
pub async fn ai_palette(state: State<'_, AppState>, query: String, app: PaletteState) -> Result<Value> {
    let (ai, key) = ai::ready(&state)?;
    let clusters: Vec<String> = app.clusters.iter().map(|c| format!("{}{}", c.name, c.env.as_deref().map(|e| format!(" ({e})")).unwrap_or_default())).collect();
    let user = format!(
        "Available clusters: {}\nKinds: {}\nNamespaces of the current clusters: {}\nCurrent view: {}\n\nRequest: {query}",
        clusters.join(", "),
        app.kinds.join(", "),
        if app.namespaces.is_empty() { "(unknown)".into() } else { app.namespaces.iter().take(300).cloned().collect::<Vec<_>>().join(", ") },
        app.current
    );
    let answer = ai::complete(&ai, key.as_deref(), PALETTE_PROMPT, &user, 400).await?;
    let v = json_object(&answer).ok_or_else(|| Error::msg("The AI didn't answer with a navigation. Try Ask AI (⌘J) instead."))?;
    let valid: Vec<&str> = app.clusters.iter().map(|c| c.name.as_str()).collect();
    let mut v = v;
    if let Some(list) = v.get_mut("clusters").and_then(Value::as_array_mut) {
        list.retain(|c| c.as_str().is_some_and(|c| valid.contains(&c)));
    }
    Ok(v)
}

/// The first JSON object in a text (models sometimes wrap it in a code block or a sentence).
fn json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str::<Value>(text.get(start..=end)?).ok().filter(Value::is_object)
}

// ---------------------------------------------------------------- terminal

const COMMAND_PROMPT: &str = "You write one shell command for the user to run in an interactive terminal, inside a Kubernetes \
container (or, for a node shell, on the node itself as root). The user reads it before pressing Enter.

Reply with the command only: one line, no explanation, no code block, no prompt sign. Chain steps with && or pipes when needed. \
Containers are often minimal: prefer POSIX sh and busybox-compatible options, and avoid tools that may be missing when a \
simpler one works. Never use sudo. Don't write destructive commands (deleting files, killing processes, changing data) unless \
the user clearly asks for that. When it can't be done with a command, reply with # followed by a short reason.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandTarget {
    pub pod: Option<String>,
    pub container: Option<String>,
    pub image: Option<String>,
    pub node: Option<String>,
    /// The last lines on the terminal screen: the shell, the OS, a previous error.
    pub screen: String,
}

#[derive(Serialize)]
pub struct Command {
    pub command: String,
}

#[tauri::command]
pub async fn ai_command(state: State<'_, AppState>, request: String, target: CommandTarget) -> Result<Command> {
    let (ai, key) = ai::ready(&state)?;
    let place = match (&target.node, &target.pod) {
        (Some(n), _) => format!("A root shell on node `{n}` (entered from a debug pod with nsenter)."),
        (None, Some(p)) => format!(
            "A shell in container `{}` of pod `{p}`{}.",
            target.container.as_deref().unwrap_or("?"),
            target.image.as_deref().map(|i| format!(", image `{i}`")).unwrap_or_default()
        ),
        _ => "A shell in a container.".into(),
    };
    let screen: Vec<&str> = target.screen.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
    let user = ai::redact(&format!("{place}\n\nThe terminal shows (last lines):\n```\n{}\n```\n\nWhat to do: {request}", ai::clip(&screen.join("\n"), 4_000)));
    let answer = ai::complete(&ai, key.as_deref(), &ai.system(COMMAND_PROMPT), &user, 300).await?;
    Ok(Command { command: one_line(&answer) })
}

/// The command alone: without code fences, a leading "$ ", or extra lines.
fn one_line(answer: &str) -> String {
    let lines: Vec<&str> = answer
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect();
    let line = if lines.len() > 1 && lines.iter().all(|l| !l.starts_with('#')) { lines.join(" && ") } else { lines.first().copied().unwrap_or("").to_string() };
    line.trim_start_matches("$ ").trim_matches('`').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_cleaned() {
        assert_eq!(one_line("```sh\ndu -sh /var/log/* | sort -h\n```"), "du -sh /var/log/* | sort -h");
        assert_eq!(one_line("$ ls -la"), "ls -la");
        assert_eq!(one_line("`df -h`"), "df -h");
        assert_eq!(one_line("cd /tmp\nls"), "cd /tmp && ls");
        assert_eq!(one_line("# no network tools in this image"), "# no network tools in this image");
    }

    /// `MOCK_AI_URL=http://127.0.0.1:18081/v1 cargo test --lib one_shot -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn one_shot_against_mock() {
        let Ok(url) = std::env::var("MOCK_AI_URL") else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        for provider in ["custom", "anthropic"] {
            let ai = crate::ai::AiSettings { enabled: true, provider: provider.into(), base_url: url.clone(), model: "m".into(), ..Default::default() };
            let nav = crate::ai::complete(&ai, Some("k"), PALETTE_PROMPT, "go", 100).await.expect(provider);
            assert_eq!(json_object(&nav).unwrap()["kind"], "Deployment");
            let cmd = crate::ai::complete(&ai, Some("k"), COMMAND_PROMPT, "biggest logs", 100).await.expect(provider);
            assert_eq!(one_line(&cmd), "du -sh /var/log/* | sort -h | tail");
        }
    }

    #[test]
    fn json_in_text() {
        assert_eq!(json_object("Sure: {\"action\": \"ask\"}.").unwrap()["action"], "ask");
        assert_eq!(json_object("```json\n{\"action\":\"navigate\",\"kind\":\"Pod\"}\n```").unwrap()["kind"], "Pod");
        assert!(json_object("no").is_none());
    }
}
