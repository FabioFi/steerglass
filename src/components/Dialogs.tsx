import { lazy, Suspense, useEffect, useState } from "react";
import { open as openFile } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { api, errText } from "../api";
import { envOf, guardProd, useStore } from "../store";
import { EnvTag } from "./EnvTag";

const CodeEditor = lazy(() => import("./YamlEditor").then((m) => ({ default: m.CodeEditor })));

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}

export function ConfirmDialog() {
  const c = useStore((s) => s.confirm);
  const [typed, setTyped] = useState("");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [checked, setChecked] = useState(false);

  useEffect(() => {
    setTyped("");
    setValue(c?.input ?? "");
    setBusy(false);
    setChecked(false);
  }, [c]);

  if (!c) return null;
  const close = () => useStore.getState().set({ confirm: null });
  const blocked = (c.typeToConfirm !== undefined && typed !== c.typeToConfirm) || busy;
  const go = async () => {
    if (blocked) return;
    setBusy(true);
    try {
      await c.onConfirm(value, checked);
    } finally {
      close();
    }
  };

  return (
    <div className="overlay" onMouseDown={close}>
      <div
        className="dialog"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") close();
          if (e.key === "Enter") go();
        }}
      >
        {c.prod && (
          <div className="prod-warning">
            <EnvTag env="prod" full /> <span className="mono">{c.prod}</span>
          </div>
        )}
        <h2>{c.title}</h2>
        {c.body && <p>{c.body}</p>}
        {c.list && <ConfirmList groups={c.list} />}
        {c.check && (
          <label className="confirm-check">
            <input type="checkbox" checked={checked} onChange={(e) => setChecked(e.target.checked)} />
            {c.check}
          </label>
        )}
        {c.input !== undefined && <input autoFocus className="text-input" value={value} onChange={(e) => setValue(e.target.value)} />}
        {c.typeToConfirm !== undefined && (
          <>
            <p className="small">
              Type <b className="mono">{c.typeToConfirm}</b> to confirm:
            </p>
            <input autoFocus className="text-input" value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} />
          </>
        )}
        <div className="dialog-actions">
          <button className="btn" onClick={close}>
            Cancel
          </button>
          <button
            className={`btn ${c.danger ? "danger" : "primary"}`}
            disabled={blocked}
            onClick={go}
            autoFocus={c.input === undefined && c.typeToConfirm === undefined}
          >
            {c.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The objects a bulk action goes to, by cluster: the first ones by name, then "and N more". */
const LIST_MAX = 10;
function ConfirmList({ groups }: { groups: { group: string; items: string[] }[] }) {
  let left = LIST_MAX;
  return (
    <div className="confirm-list">
      {groups.map(({ group, items }) => {
        const shown = items.slice(0, Math.max(0, left));
        left -= shown.length;
        const more = items.length - shown.length;
        return (
          <div key={group}>
            <div className="confirm-group">
              <EnvTag env={envOf(group)} /> <span className="mono">{group}</span> <span className="muted">· {items.length}</span>
            </div>
            {shown.map((n) => (
              <div key={n} className="mono confirm-item">
                {n}
              </div>
            ))}
            {more > 0 && <div className="muted confirm-item">and {more} more</div>}
          </div>
        );
      })}
    </div>
  );
}

export async function grant(path: string) {
  try {
    await api.grantAccess(path);
    await useStore.getState().loadContexts(true);
  } catch (e) {
    useStore.getState().toast(errText(e), "error");
  }
}

/** Inside the Mac App Store sandbox ~/.kube is only readable after the user grants access once. */
export function GrantKubeDirButton({ primary }: { primary?: boolean }) {
  const [env, setEnv] = useState<{ sandboxed: boolean; kubeDir?: string } | null>(null);
  const report = useStore((s) => s.report);
  useEffect(() => {
    api.environment().then(setEnv);
  }, []);
  if (!env || (!env.sandboxed && (report?.contexts.length ?? 0) > 0)) return null;
  return (
    <button
      className={`btn ${primary ? "primary" : ""}`}
      onClick={async () => {
        const path = await openFile({ directory: true, defaultPath: env.kubeDir, title: "Grant access to your .kube folder" });
        if (typeof path === "string") await grant(path);
      }}
    >
      <FolderOpen size={13} /> Grant access to ~/.kube…
    </button>
  );
}

const TEMPLATE = `apiVersion: v1
kind: ConfigMap
metadata:
  name: example
data:
  hello: world
`;

export function ApplyDialog() {
  const open = useStore((s) => s.applyOpen);
  const contexts = useStore((s) => s.contexts);
  const namespace = useStore((s) => s.namespace);
  const report = useStore((s) => s.report);
  const [yaml, setYaml] = useState(TEMPLATE);
  const [ctx, setCtx] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { set, toast } = useStore.getState();

  const draft = useStore((s) => s.applyDraft);
  useEffect(() => {
    if (open) {
      setCtx(draft?.context ?? contexts[0] ?? report?.contexts.find((c) => c.isCurrent)?.name ?? report?.contexts[0]?.name ?? "");
      if (draft) setYaml(draft.yaml);
      setError(null);
    }
  }, [open, contexts, report, draft]);

  if (!open) return null;
  const close = () => set({ applyOpen: false, applyDraft: null });
  const apply = () =>
    guardProd(ctx, `Apply to ${ctx}?`, "The objects in the manifest are created or updated on this production cluster.", "Apply", doApply);
  const doApply = async () => {
    setBusy(true);
    try {
      const applied = await api.applyManifest(ctx, yaml, namespace || undefined);
      toast(`Applied ${applied.join(", ")}`, "success");
      close();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="overlay" onMouseDown={close}>
      <div className="dialog wide tall" onMouseDown={(e) => e.stopPropagation()} onKeyDown={(e) => e.key === "Escape" && close()}>
        <div className="dialog-head">
          <h2>Apply YAML</h2>
          {draft && <span className="muted small">Proposed by AI: review it before applying</span>}
          <select value={ctx} onChange={(e) => setCtx(e.target.value)}>
            {report?.contexts.map((c) => (
              <option key={c.name} value={c.name}>
                {c.name}
              </option>
            ))}
          </select>
          <span className="muted small">namespace: {namespace || "from manifest / default"}</span>
        </div>
        {error && <div className="error-banner">{error}</div>}
        <Suspense fallback={<div className="code" />}>
          <CodeEditor value={yaml} onChange={setYaml} onSave={apply} />
        </Suspense>
        <div className="dialog-actions">
          <button className="btn" onClick={close}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || !ctx} onClick={apply}>
            {busy ? "Applying…" : "Apply (server-side)"}
          </button>
        </div>
      </div>
    </div>
  );
}
