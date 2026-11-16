import { useState } from "react";
import { Copy, MessageSquare, RotateCcw, Settings as SettingsIcon, Sparkles, Square } from "lucide-react";
import { api, errText, type ObjRef } from "../api";
import { aiBlocked, canExplain, destinationOf, explain, followUp, objKey, runAi, stopAi, useAi } from "../ai";
import { useStore } from "../store";
import { Markdown } from "../markdown";

const openAiSettings = () => useStore.getState().set({ settingsOpen: true, settingsSection: "ai" });

/** What "Explain" sends, in words. */
function sends(r: ObjRef) {
  if (r.kind.group === "" && r.kind.kind === "Pod") return "its YAML, recent events and latest log lines";
  if (["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet", "Job", "Service"].includes(r.kind.kind)) return "its YAML, recent events, its pods and the logs of one in trouble";
  return "its YAML and recent events";
}

export function AiPanel({ objRef }: { objRef: ObjRef }) {
  const key = objKey(objRef);
  const run = useAi((s) => s.runs[key]);
  const settings = useAi((s) => s.settings);
  const [preview, setPreview] = useState<string | null>(null);
  const [loadingPreview, setLoadingPreview] = useState(false);
  const blocked = aiBlocked(objRef.context);
  const kind = objRef.kind.kind;

  if (!canExplain(objRef)) return <div className="pad muted">Secrets are never sent to an AI provider.</div>;
  if (blocked && !run)
    return (
      <div className="ai-intro">
        <Sparkles size={22} className="ai-icon" />
        <p className="muted">{blocked}</p>
        <button className="btn" onClick={openAiSettings}>
          <SettingsIcon size={13} /> AI settings
        </button>
      </div>
    );

  const showPreview = async () => {
    if (preview !== null) return setPreview(null);
    setLoadingPreview(true);
    try {
      setPreview((await api.aiPrepare({ type: "explain", context: objRef.context, kind: objRef.kind, namespace: objRef.namespace, name: objRef.name })).text);
    } catch (e) {
      useStore.getState().toast(errText(e), "error");
    } finally {
      setLoadingPreview(false);
    }
  };

  if (!run)
    return (
      <div className="ai-intro">
        <Sparkles size={22} className="ai-icon" />
        <h3>Explain this {kind.toLowerCase()}</h3>
        <p className="muted">
          Sends {sends(objRef)} to <b>{settings ? destinationOf(settings) : ""}</b>, which tells you what is wrong and what to do. Values that look like
          passwords or tokens are hidden.
        </p>
        <div className="row-gap">
          <button className="btn primary" onClick={() => explain(objRef)}>
            <Sparkles size={13} /> Explain
          </button>
          <button className="btn" onClick={showPreview} disabled={loadingPreview}>
            {loadingPreview ? "Collecting…" : preview !== null ? "Hide what is sent" : "Show what is sent"}
          </button>
        </div>
        {preview !== null && <pre className="ai-sent">{preview}</pre>}
      </div>
    );

  const running = run.status === "running";
  return (
    <div className="ai">
      <div className="subbar">
        <Sparkles size={13} className="ai-icon" />
        <b className="ai-title">{run.title}</b>
        <div className="spacer" />
        {running ? (
          <button className="btn small" onClick={() => stopAi(key)}>
            <Square size={11} /> Stop
          </button>
        ) : (
          <>
            {run.text && (
              <button
                className="icon-btn"
                title="Copy the answer"
                onClick={() => {
                  navigator.clipboard.writeText(run.text);
                  useStore.getState().toast("Copied the answer");
                }}
              >
                <Copy size={13} />
              </button>
            )}
            {run.status === "done" && run.text && run.sent && (
              <button className="btn small" onClick={() => followUp(run)} title="Continue in Ask AI: the AI can read more (other pods, other clusters)">
                <MessageSquare size={12} /> Ask a follow-up
              </button>
            )}
            <button className="btn small" onClick={() => runAi(key, run.title, run.task)} disabled={!!blocked} title={blocked ?? "Ask again with fresh data"}>
              <RotateCcw size={12} /> Ask again
            </button>
          </>
        )}
      </div>
      <div className="ai-body">
        {running && !run.text && <div className="muted">{run.sent ? "Thinking…" : "Collecting data…"}</div>}
        {run.text && (
          <div className="md">
            <Markdown text={run.text} />
            {running && <span className="ai-cursor" />}
          </div>
        )}
        {run.error && (
          <div className="error ai-error">
            {run.error}
            {/Settings → AI/.test(run.error) && (
              <button className="btn small" onClick={openAiSettings}>
                AI settings
              </button>
            )}
          </div>
        )}
        <div className="ai-foot muted small">
          {run.destination && <span>Answered by {run.destination}. </span>}AI can be wrong: check before you change anything.
        </div>
        {run.sent && (
          <details className="ai-details">
            <summary className="small muted">What was sent ({run.sent.length.toLocaleString()} characters)</summary>
            <pre className="ai-sent">{run.sent}</pre>
          </details>
        )}
      </div>
    </div>
  );
}
