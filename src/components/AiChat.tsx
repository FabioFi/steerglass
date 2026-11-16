import { useEffect, useRef, useState } from "react";
import { AlertCircle, ArrowUp, Check, Loader2, Plus, Settings as SettingsIcon, Sparkles, Square, Wrench } from "lucide-react";
import { applyProposal, chatContexts, destinationOf, proposalTitle, resetChat, sendChat, stopChat, useAi, type ChatItem, type ChatPart } from "../ai";
import { EnvTag } from "./EnvTag";
import { envOf } from "../store";
import { useStore } from "../store";
import { Markdown } from "../markdown";

const SUGGESTIONS = [
  "What needs attention in my clusters right now?",
  "Which pods restarted in the last hour, and why?",
  "Summarize the warning events of the last hour",
  "Compare the deployments of two clusters and tell me what differs",
];

const openAiSettings = () => useStore.getState().set({ settingsOpen: true, settingsSection: "ai" });

/** "Ask AI": a conversation in which the model reads the clusters with read-only tools. */
export function AiChat() {
  const chat = useAi((s) => s.chat);
  const settings = useAi((s) => s.settings);
  useStore((s) => s.report); // the list of clusters below follows the kubeconfig
  useStore((s) => s.envs);
  const [text, setText] = useState("");
  const bottom = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const { contexts, hiddenProduction } = chatContexts();

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [chat]);
  useEffect(() => input.current?.focus(), [chat.id]);

  const send = (t = text) => {
    if (!t.trim() || chat.running) return;
    sendChat(t.trim());
    setText("");
  };

  const ready = settings?.enabled && settings.model.trim();
  return (
    <div className="ai-chat">
      <header className="toolbar" data-tauri-drag-region>
        <Sparkles size={15} className="ai-icon" />
        <h1 className="ai-chat-title">Ask AI</h1>
        <span className="muted small">{settings ? destinationOf(settings) : ""}</span>
        <div className="spacer" data-tauri-drag-region />
        <button className="btn" onClick={resetChat} disabled={!chat.items.length && !chat.seed}>
          <Plus size={13} /> New chat
        </button>
      </header>

      <div className="ai-chat-body">
        <div className="ai-chat-col">
          {!chat.items.length && (
            <div className="ai-chat-empty">
              <Sparkles size={26} className="ai-icon" />
              <h2>Ask about your clusters</h2>
              <p className="muted">
                The AI reads what it needs with read-only tools (resources, YAML, events, logs) and can compare clusters. It never changes anything: when a
                change is needed it shows you the command.
              </p>
              <p className="muted small">
                It can read {contexts.length ? contexts.map((c) => c.name).join(", ") : "no clusters"}.
                {hiddenProduction > 0 && ` ${hiddenProduction} production cluster${hiddenProduction > 1 ? "s are" : " is"} left out (Settings → AI).`}
              </p>
              {ready ? (
                <div className="ai-suggestions">
                  {SUGGESTIONS.map((s) => (
                    <button key={s} className="btn" onClick={() => send(s)}>
                      {s}
                    </button>
                  ))}
                </div>
              ) : (
                <button className="btn" onClick={openAiSettings}>
                  <SettingsIcon size={13} /> {settings?.enabled ? "Choose a model" : "Turn on AI"} in Settings
                </button>
              )}
            </div>
          )}
          {chat.items.map((item, i) => (
            <Item key={i} item={item} running={chat.running && i === chat.items.length - 1} />
          ))}
          <div ref={bottom} />
        </div>
      </div>

      <div className="ai-chat-input">
        <div className="ai-chat-col">
          <div className="ai-chat-box">
            <textarea
              ref={input}
              value={text}
              rows={Math.min(8, Math.max(1, text.split("\n").length))}
              placeholder={chat.items.length ? "Ask a follow-up…" : "Ask about your clusters…"}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  send();
                }
              }}
              disabled={!ready}
              aria-label="Message"
            />
            {chat.running ? (
              <button className="ai-send" onClick={stopChat} title="Stop" aria-label="Stop">
                <Square size={13} />
              </button>
            ) : (
              <button className="ai-send" onClick={() => send()} disabled={!text.trim() || !ready} title="Send (Enter)" aria-label="Send">
                <ArrowUp size={15} />
              </button>
            )}
          </div>
          <div className="muted small ai-chat-note">Read-only. AI can be wrong: check before you change anything. Enter sends, Shift+Enter adds a line.</div>
        </div>
      </div>
    </div>
  );
}

function Item({ item, running }: { item: ChatItem; running: boolean }) {
  if (item.role === "user") return <div className="ai-msg user">{item.text}</div>;
  return (
    <div className="ai-msg assistant">
      {item.parts.map((p, i) =>
        p.type === "text" ? (
          <div key={i} className="md">
            <Markdown text={p.text} />
          </div>
        ) : p.type === "proposal" ? (
          <ProposalCard key={i} part={p} />
        ) : (
          <div key={i} className={`ai-tool ${p.status}`}>
            {p.status === "running" ? <Loader2 size={12} className="spin" /> : p.status === "ok" ? <Check size={12} /> : <AlertCircle size={12} />}
            <span>{p.label}</span>
            {p.summary && <span className="muted">· {p.summary}</span>}
          </div>
        ),
      )}
      {running && !item.parts.some((p) => p.type === "text") && !item.parts.some((p) => p.type === "tool" && p.status === "running") && (
        <div className="muted small">Thinking…</div>
      )}
      {running && <span className="ai-cursor" />}
      {item.error && (
        <div className="error ai-error">
          {item.error}
          {/Settings → AI/.test(item.error) && (
            <button className="btn small" onClick={openAiSettings}>
              AI settings
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function ProposalCard({ part }: { part: Extract<ChatPart, { type: "proposal" }> }) {
  const p = part.proposal;
  const where = `${p.cluster}${p.namespace ? ` / ${p.namespace}` : ""}`;
  return (
    <div className="ai-proposal">
      <div className="ai-proposal-head">
        <Wrench size={13} />
        <b>{proposalTitle(p)}</b>
        <span className="muted small">{where}</span>
        <EnvTag env={envOf(p.cluster)} />
      </div>
      {p.reason && <div className="small">{p.reason}</div>}
      {p.manifest && <pre className="ai-sent">{p.manifest}</pre>}
      <div className="row-gap">
        {part.state === "applied" ? (
          <span className="small ok-text">
            <Check size={12} /> Applied
          </span>
        ) : (
          <button className={`btn small ${part.state === "pending" ? "primary" : ""}`} onClick={() => applyProposal(p)}>
            {p.action === "apply" ? (part.state === "review" ? "Open again…" : "Review and apply…") : part.state === "failed" ? "Try again" : "Apply…"}
          </button>
        )}
        {part.error && <span className="error small">{part.error}</span>}
        <span className="muted small">Proposed by AI · you confirm before anything changes</span>
      </div>
    </div>
  );
}

