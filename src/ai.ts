import { create } from "zustand";
import { api, errText, type AiProvider, type AiSettings, type AiTask, type ObjRef, type Proposal } from "./api";
import { envOf, isProdLike, useStore } from "./store";
import { ENV_NAME } from "./env";

/** Providers offered in Settings. Every one but Anthropic speaks the OpenAI chat API. */
export const PROVIDERS: { id: AiProvider; label: string; base: string; key: "required" | "optional" | "none"; hint: string }[] = [
  { id: "openai", label: "OpenAI", base: "https://api.openai.com/v1", key: "required", hint: "Create a key at platform.openai.com/api-keys." },
  { id: "anthropic", label: "Anthropic (Claude)", base: "https://api.anthropic.com/v1", key: "required", hint: "Create a key at console.anthropic.com." },
  { id: "ollama", label: "Ollama (on this computer)", base: "http://localhost:11434/v1", key: "none", hint: "Nothing leaves your computer. Pull a model first, e.g. ollama pull qwen3." },
  { id: "lmstudio", label: "LM Studio (on this computer)", base: "http://localhost:1234/v1", key: "none", hint: "Nothing leaves your computer. Start the server in LM Studio's Developer tab." },
  { id: "deepseek", label: "DeepSeek", base: "https://api.deepseek.com/v1", key: "required", hint: "Create a key at platform.deepseek.com." },
  { id: "mistral", label: "Mistral", base: "https://api.mistral.ai/v1", key: "required", hint: "Create a key at console.mistral.ai." },
  { id: "openrouter", label: "OpenRouter", base: "https://openrouter.ai/api/v1", key: "required", hint: "One key for many models (Qwen, Gemini, Llama…): openrouter.ai/keys." },
  { id: "custom", label: "Other (OpenAI-compatible server)", base: "", key: "optional", hint: "Any server with the OpenAI chat API: vLLM, LiteLLM, a company gateway…" },
];

export const providerOf = (id: AiProvider) => PROVIDERS.find((p) => p.id === id) ?? PROVIDERS[0];

/** "api.openai.com · gpt-…": where requests go. */
export function destinationOf(ai: AiSettings) {
  const base = ai.baseUrl.trim() || providerOf(ai.provider).base;
  const host = base.replace(/^[a-z]+:\/\//i, "").split("/")[0];
  return `${host || "no server set"} · ${ai.model || "no model chosen"}`;
}

export interface AiRun {
  title: string;
  task: AiTask;
  status: "running" | "done" | "error";
  text: string;
  /** Exactly what was sent, and where. */
  sent?: string;
  destination?: string;
  error?: string;
  streamId?: number;
}

export type ChatPart =
  | { type: "text"; text: string }
  | { type: "tool"; id: string; label: string; status: "running" | "ok" | "error"; summary?: string }
  | { type: "proposal"; proposal: Proposal; state: "pending" | "applied" | "review" | "failed"; error?: string };
export type ChatItem = { role: "user"; text: string } | { role: "assistant"; parts: ChatPart[]; error?: string };

export interface Chat {
  id: number;
  items: ChatItem[];
  running: boolean;
  streamId?: number;
  /** Set when the conversation starts from an Explain answer: sent with the first question. */
  seed?: { question: string; answer: string };
}

interface AiState {
  settings: AiSettings | null;
  /** The latest answer per object (see `objKey`), kept while the app runs. */
  runs: Record<string, AiRun>;
  chat: Chat;
}

const newChat = (): Chat => ({ id: Date.now(), items: [], running: false });

export const useAi = create<AiState>(() => ({ settings: null, runs: {}, chat: newChat() }));

export async function loadAiSettings() {
  try {
    useAi.setState({ settings: (await api.getSettings()).ai });
  } catch {
    /* AI stays off */
  }
}

export async function saveAiSettings(ai: AiSettings) {
  useAi.setState({ settings: ai });
  await api.aiUpdateSettings(ai);
}

export const objKey = (r: ObjRef) => `${r.context}|${r.kind.group}|${r.kind.kind}|${r.namespace ?? ""}|${r.name}`;

export const aiEnabled = () => !!useAi.getState().settings?.enabled;

/** Why AI can't be used on a cluster right now, or null when it can. */
export function aiBlocked(context: string): string | null {
  const s = useAi.getState().settings;
  if (!s?.enabled) return "AI is off. Turn it on in Settings → AI.";
  if (!s.onProduction && isProdLike(context)) return "AI isn't used on production clusters, so their data stays on your computer. You can allow it in Settings → AI.";
  if (!s.model.trim()) return "Choose a model in Settings → AI.";
  return null;
}

const isSecret = (r: ObjRef) => r.kind.group === "" && r.kind.kind === "Secret";
export const canExplain = (r: ObjRef) => !isSecret(r);

const patch = (key: string, p: Partial<AiRun>) =>
  useAi.setState((s) => (s.runs[key] ? { runs: { ...s.runs, [key]: { ...s.runs[key], ...p } } } : s));

export function stopAi(key: string) {
  const run = useAi.getState().runs[key];
  if (run?.streamId !== undefined && run.status === "running") api.stopStream(run.streamId);
  patch(key, { status: "done" });
}

/** Sends a task and streams the answer into `runs[key]`. */
export function runAi(key: string, title: string, task: AiTask) {
  stopAi(key);
  let text = "";
  let frame = 0;
  useAi.setState((s) => ({ runs: { ...s.runs, [key]: { title, task, status: "running", text: "" } } }));
  // Many small pieces arrive per frame: render once per frame.
  const flush = () => {
    frame = 0;
    patch(key, { text });
  };
  api
    .aiRun(task, (m) => {
      if (useAi.getState().runs[key]?.task !== task) return;
      if (m.type === "sent") patch(key, { sent: m.text, destination: m.destination });
      else if (m.type === "delta") {
        text += m.text;
        if (!frame) frame = requestAnimationFrame(flush);
      } else {
        cancelAnimationFrame(frame);
        patch(key, m.type === "done" ? { text, status: "done" } : { text, status: "error", error: m.message });
      }
    })
    .then((streamId) => patch(key, { streamId }))
    .catch((e) => patch(key, { status: "error", error: errText(e) }));
}

export function explainTask(r: ObjRef): AiTask {
  return { type: "explain", context: r.context, kind: r.kind, namespace: r.namespace, name: r.name };
}

/** Opens the object's AI tab and explains it. */
export function explain(r: ObjRef) {
  const blocked = aiBlocked(r.context);
  const st = useStore.getState();
  st.openDetail(r, "ai");
  if (blocked || !canExplain(r)) return;
  runAi(objKey(r), `Explanation of ${r.kind.kind.toLowerCase()} ${r.name}`, explainTask(r));
}

/** Summarizes log lines of a pod in its AI tab. */
export function summarizeLogs(r: ObjRef, container: string | undefined, lines: string[]) {
  const blocked = aiBlocked(r.context);
  if (blocked) return useStore.getState().toast(blocked, "error");
  if (!lines.length) return useStore.getState().toast("There are no log lines to summarize yet.");
  runAi(objKey(r), `Log summary${container ? ` of ${container}` : ""}`, { type: "logs", pod: r.name, container, lines });
  useStore.getState().setDetailTab("ai");
}

// ---------------------------------------------------------------- chat

/** The clusters the AI may read: all of them, or all but production unless allowed. */
export function chatContexts() {
  const { report } = useStore.getState();
  const s = useAi.getState().settings;
  const all = report?.contexts.map((c) => c.name) ?? [];
  const allowed = all.filter((c) => s?.onProduction || !isProdLike(c));
  return {
    contexts: allowed.map((name) => {
      const env = envOf(name);
      return { name, env: env ? ENV_NAME[env] : null };
    }),
    hiddenProduction: all.length - allowed.length,
  };
}

const patchChat = (id: number, fn: (c: Chat) => Partial<Chat>) =>
  useAi.setState((s) => (s.chat.id === id ? { chat: { ...s.chat, ...fn(s.chat) } } : s));

/** Applies `fn` to the last assistant item of the chat. */
const patchAnswer = (id: number, fn: (a: Extract<ChatItem, { role: "assistant" }>) => Partial<Extract<ChatItem, { role: "assistant" }>>) =>
  patchChat(id, (c) => {
    const items = [...c.items];
    const last = items[items.length - 1];
    if (last?.role === "assistant") items[items.length - 1] = { ...last, ...fn(last) };
    return { items };
  });

export function resetChat() {
  const { chat } = useAi.getState();
  if (chat.running && chat.streamId !== undefined) api.stopStream(chat.streamId);
  api.aiChatReset(chat.id);
  useAi.setState({ chat: newChat() });
}

export function stopChat() {
  const { chat } = useAi.getState();
  if (chat.streamId !== undefined) api.stopStream(chat.streamId);
  patchChat(chat.id, () => ({ running: false }));
}

export function sendChat(text: string) {
  const { chat, settings } = useAi.getState();
  if (!text.trim() || chat.running) return;
  if (!settings?.enabled) return useStore.getState().toast("AI is off. Turn it on in Settings → AI.", "error");
  if (!settings.model.trim()) return useStore.getState().toast("Choose a model in Settings → AI.", "error");
  const id = chat.id;
  const seed = chat.seed;
  patchChat(id, (c) => ({ running: true, seed: undefined, items: [...c.items, { role: "user", text }, { role: "assistant", parts: [] }] }));
  let pending = "";
  let frame = 0;
  const flush = () => {
    frame = 0;
    const add = pending;
    pending = "";
    if (!add) return;
    patchAnswer(id, (a) => {
      const parts = [...a.parts];
      const last = parts[parts.length - 1];
      if (last?.type === "text") parts[parts.length - 1] = { ...last, text: last.text + add };
      else parts.push({ type: "text", text: add });
      return { parts };
    });
  };
  api
    .aiChatSend({ chat: id, text, seed, ...chatContexts() }, (m) => {
      if (m.type === "delta") {
        pending += m.text;
        if (!frame) frame = requestAnimationFrame(flush);
        return;
      }
      cancelAnimationFrame(frame);
      flush();
      if (m.type === "proposal") patchAnswer(id, (a) => ({ parts: [...a.parts, { type: "proposal", proposal: m.proposal, state: "pending" }] }));
      else if (m.type === "toolStart") patchAnswer(id, (a) => ({ parts: [...a.parts, { type: "tool", id: m.id, label: m.label, status: "running" }] }));
      else if (m.type === "toolEnd")
        patchAnswer(id, (a) => ({ parts: a.parts.map((p) => (p.type === "tool" && p.id === m.id ? { ...p, status: m.ok ? "ok" : "error", summary: m.summary } : p)) }));
      else {
        if (m.type === "error") patchAnswer(id, () => ({ error: m.message }));
        patchChat(id, () => ({ running: false }));
      }
    })
    .then((streamId) => patchChat(id, () => ({ streamId })))
    .catch((e) => {
      patchAnswer(id, () => ({ error: errText(e) }));
      patchChat(id, () => ({ running: false }));
    });
}

/** Continues an Explain answer as a conversation, in the Ask AI view. */
export function followUp(run: AiRun) {
  if (!run.sent || !run.text) return;
  resetChat();
  useAi.setState((s) => ({
    chat: { ...s.chat, seed: { question: run.sent!, answer: run.text }, items: [{ role: "assistant", parts: [{ type: "text", text: `**${run.title}**\n\n${run.text}` }] }] },
  }));
  useStore.getState().set({ view: "ai" });
}

// ---------------------------------------------------------------- proposed changes

/** "Scale deployment web to 3", in words. */
export function proposalTitle(p: Proposal) {
  const what = `${p.kind?.kind.toLowerCase() ?? ""} ${p.name ?? ""}`.trim();
  switch (p.action) {
    case "scale":
      return `Scale ${what} to ${p.replicas}`;
    case "restart":
      return `Rollout restart ${what}`;
    case "cordon":
      return `Cordon ${what}`;
    case "uncordon":
      return `Uncordon ${what}`;
    case "suspend":
      return `Suspend ${what}`;
    case "resume":
      return `Resume ${what}`;
    case "apply":
      return "Apply YAML";
  }
}

const setProposal = (proposalId: string, p: Partial<Extract<ChatPart, { type: "proposal" }>>) =>
  useAi.setState((s) => ({
    chat: {
      ...s.chat,
      items: s.chat.items.map((it) =>
        it.role === "assistant" ? { ...it, parts: it.parts.map((x) => (x.type === "proposal" && x.proposal.id === proposalId ? { ...x, ...p } : x)) } : it,
      ),
    },
  }));

/** Applies a proposed change after the user confirms it (always asked, in red on production). */
export function applyProposal(p: Proposal) {
  const st = useStore.getState();
  if (p.action === "apply") {
    st.set({ applyOpen: true, applyDraft: { yaml: p.manifest ?? "", context: p.cluster } });
    setProposal(p.id, { state: "review" });
    return;
  }
  const ref: ObjRef = { context: p.cluster, kind: p.kind!, namespace: p.namespace ?? undefined, name: p.name! };
  const run = async () => {
    switch (p.action) {
      case "scale":
        return api.scale(ref, p.replicas!);
      case "restart":
        return api.rolloutRestart(ref);
      case "cordon":
      case "uncordon":
        return api.setCordon(p.cluster, p.name!, p.action === "cordon");
      case "suspend":
      case "resume":
        return api.suspendCronjob(p.cluster, p.namespace!, p.name!, p.action === "suspend");
    }
  };
  const where = `${p.cluster}${p.namespace ? ` / ${p.namespace}` : ""}`;
  st.ask({
    title: `${proposalTitle(p)}?`,
    body: `Proposed by AI${p.reason ? `: ${p.reason}` : "."} Cluster: ${where}.`,
    confirmLabel: "Apply",
    prod: isProdLike(p.cluster) ? p.cluster : undefined,
    onConfirm: async () => {
      try {
        await run();
        setProposal(p.id, { state: "applied" });
        st.toast(`${proposalTitle(p)}: done`, "success");
      } catch (e) {
        setProposal(p.id, { state: "failed", error: errText(e) });
        st.toast(errText(e), "error");
      }
    },
  });
}
