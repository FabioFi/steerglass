import { useEffect, useMemo, useRef, useState } from "react";
import { envOf, useStore } from "../store";
import { api, errText } from "../api";
import { sendChat, useAi } from "../ai";
import { ENV_NAME } from "../env";
import { Sparkles } from "lucide-react";
import { BUILTIN, isBuiltin } from "../kinds";
import { actionsFor } from "../actions";
import { fuzzy, shortContext } from "../util";

interface Item {
  id: string;
  group: string;
  label: string;
  hint?: string;
  keywords?: string;
  run: () => void;
}

/** ⌘K in plain words: the AI turns the request into a navigation, or hands a real question to Ask AI. */
async function askPalette(query: string) {
  const s = useStore.getState();
  const nss = new Set<string>();
  s.contexts.forEach((c) => s.namespaces[c]?.forEach((n) => nss.add(n)));
  const discovered = s.discovered[s.contexts[0]] ?? [];
  const kinds = [...new Set([...BUILTIN.map((b) => b.ref.kind), ...discovered.map((k) => k.kind)])];
  const a = await api.aiPalette(query, {
    clusters: (s.report?.contexts ?? []).map((c) => {
      const env = envOf(c.name);
      return { name: c.name, env: env ? ENV_NAME[env] : null };
    }),
    kinds,
    namespaces: [...nss].sort(),
    current: { view: s.view, clusters: s.contexts, kind: s.kind.kind, namespace: s.namespace || "(all namespaces)", filter: s.filter },
  });
  s.set({ paletteOpen: false });
  if (a.action !== "navigate") {
    s.set({ view: "ai" });
    sendChat(query);
    return;
  }
  const clusters = a.clusters?.length ? a.clusters : s.contexts.length ? s.contexts : (s.report?.contexts.slice(0, 1).map((c) => c.name) ?? []);
  const want = a.kind?.toLowerCase();
  const kind = want
    ? (BUILTIN.find((b) => b.ref.kind.toLowerCase() === want || b.label.toLowerCase() === want)?.ref ??
      (s.discovered[clusters[0]] ?? discovered).find((k) => k.kind.toLowerCase() === want || k.plural === want))
    : undefined;
  await s.openResources(clusters, kind, a.namespace);
  if (a.filter) s.setFilter(a.filter);
  const k = kind ?? useStore.getState().kind;
  if (a.open && clusters.length === 1 && (!k.namespaced || a.namespace)) s.openDetail({ context: clusters[0], kind: k, namespace: a.namespace || undefined, name: a.open });
}

export function CommandPalette() {
  const open = useStore((s) => s.paletteOpen);
  const aiOn = useAi((s) => !!s.settings?.enabled && !!s.settings.model.trim());
  const [busy, setBusy] = useState(false);
  const [q, setQ] = useState("");
  const [idx, setIdx] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) {
      setQ("");
      setIdx(0);
      const st = useStore.getState();
      if (st.view === "resources") st.contexts.forEach((c) => st.ensureNamespaces(c, true));
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  // Namespaces are read again when the palette opens: rebuild the list when they arrive.
  const namespaces = useStore((s) => s.namespaces);
  const items = useMemo<Item[]>(() => {
    if (!open) return [];
    const s = useStore.getState();
    const close = () => s.set({ paletteOpen: false });
    const wrap = (fn: () => void) => () => {
      close();
      fn();
    };
    const out: Item[] = [];

    // Actions on the selected row come first: they're the most likely intent.
    const sel = s.view === "resources" ? s.selectedRef : null;
    if (sel) {
      for (const a of actionsFor(sel)) {
        out.push({ id: `act-${a.id}`, group: `Action · ${sel.name}`, label: a.label, hint: a.shortcut, run: wrap(a.run) });
      }
    }

    for (const b of BUILTIN) {
      out.push({ id: `kind-${b.label}`, group: "Resource", label: b.label, hint: b.aliases[0], keywords: b.aliases.join(" "), run: wrap(() => s.setKind(b.ref)) });
    }
    const discovered = s.discovered[s.contexts[0]] ?? [];
    for (const k of discovered) {
      if (isBuiltin(k)) continue;
      out.push({ id: `crd-${k.group}-${k.plural}`, group: "Resource", label: k.kind, hint: k.group, keywords: `${k.plural} ${k.group}`, run: wrap(() => s.setKind(k)) });
    }
    for (const c of s.report?.contexts ?? []) {
      out.push({ id: `ctx-${c.name}`, group: "Cluster", label: shortContext(c.name), hint: c.name !== shortContext(c.name) ? c.name : undefined, keywords: c.name, run: wrap(() => s.openResources([c.name])) });
      if (s.view === "resources" && !s.contexts.includes(c.name)) {
        out.push({ id: `ctx-add-${c.name}`, group: "Add cluster to view", label: shortContext(c.name), keywords: `add ${c.name}`, run: wrap(() => s.toggleContext(c.name, true)) });
      }
    }
    if (s.view === "resources") {
      const nss = new Set<string>();
      s.contexts.forEach((c) => s.namespaces[c]?.forEach((n) => nss.add(n)));
      out.push({ id: "ns-all", group: "Namespace", label: "All namespaces", keywords: "ns all", run: wrap(() => s.setNamespace("")) });
      for (const n of [...nss].sort()) out.push({ id: `ns-${n}`, group: "Namespace", label: n, keywords: `ns ${n}`, run: wrap(() => s.setNamespace(n)) });
    }
    out.push(
      { id: "go-overview", group: "Go to", label: "Overview", run: wrap(() => s.set({ view: "overview" })) },
      { id: "apply", group: "Command", label: "Apply YAML…", run: wrap(() => s.set({ applyOpen: true })) },
      { id: "settings", group: "Command", label: "Settings…", run: wrap(() => s.set({ settingsOpen: true })) },
      { id: "help", group: "Command", label: "Help & keyboard shortcuts", hint: "?", keywords: "shortcuts keys guide", run: wrap(() => s.set({ helpOpen: true })) },
      { id: "reload", group: "Command", label: "Reload kubeconfig", run: wrap(() => s.loadContexts(true)) },
      { id: "probe", group: "Command", label: "Check all clusters", run: wrap(() => s.probeAll()) },
      { id: "failing", group: "Command", label: "Show failing only", keywords: "error unhealthy", run: wrap(() => s.setFilter("status:bad")) },
    );
    return out;
  }, [open, namespaces]);

  const runAi = async (query: string) => {
    if (busy || !query.trim()) return;
    setBusy(true);
    try {
      await askPalette(query.trim());
    } catch (e) {
      useStore.getState().toast(errText(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const results = useMemo(() => {
    const query = q.trim().toLowerCase();
    if (!query) return items.slice(0, 200);
    const ai: Item[] =
      aiOn && query.length >= 3
        ? [{ id: "ai", group: "AI", label: `Ask AI: “${q.trim()}”`, hint: "⇧↵", run: () => runAi(q) }]
        : [];
    const found = items
      .map((it) => ({ it, score: Math.max(fuzzy(query, it.label), fuzzy(query, it.keywords ?? "") - 1, fuzzy(query, `${it.group} ${it.label}`) - 2) }))
      .filter((x) => x.score >= 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 200)
      .map((x) => x.it);
    // Plain words that match nothing go to the AI first.
    return found.length ? [...found, ...ai] : ai;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items, q, aiOn]);

  useEffect(() => setIdx(0), [q]);
  useEffect(() => {
    listRef.current?.querySelector(".pal-item.active")?.scrollIntoView({ block: "nearest" });
  }, [idx]);

  if (!open) return null;
  const close = () => useStore.getState().set({ paletteOpen: false });

  return (
    <div className="overlay" onMouseDown={close}>
      <div className="palette" onMouseDown={(e) => e.stopPropagation()}>
        <input
          ref={inputRef}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder={aiOn ? "Type a resource, cluster, namespace or command, or ask in plain words (⇧↵)…" : "Type a resource, cluster, namespace or command…"}
          disabled={busy}
          spellCheck={false}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" || (e.key === "n" && e.ctrlKey)) {
              e.preventDefault();
              setIdx((i) => Math.min(results.length - 1, i + 1));
            } else if (e.key === "ArrowUp" || (e.key === "p" && e.ctrlKey)) {
              e.preventDefault();
              setIdx((i) => Math.max(0, i - 1));
            } else if (e.key === "Enter" && e.shiftKey && aiOn) {
              e.preventDefault();
              runAi(q);
            } else if (e.key === "Enter") {
              e.preventDefault();
              results[idx]?.run();
            } else if (e.key === "Escape") {
              e.preventDefault();
              close();
            }
          }}
        />
        <div className="pal-list" ref={listRef}>
          {results.map((r, i) => (
            // onMouseMove, not onMouseEnter: the list re-renders under a still cursor while typing,
            // which must not steal the keyboard selection.
            <div key={r.id} className={`pal-item ${i === idx ? "active" : ""}`} onMouseMove={() => i !== idx && setIdx(i)} onClick={r.run}>
              <span className="pal-group">{r.group}</span>
              <span className="pal-label">{r.label}</span>
              {r.hint && <span className="pal-hint">{r.hint}</span>}
            </div>
          ))}
          {busy && (
            <div className="empty-small">
              <Sparkles size={12} className="ai-icon" /> Asking AI…
            </div>
          )}
          {!busy && results.length === 0 && <div className="empty-small">No results</div>}
        </div>
      </div>
    </div>
  );
}
