import { api, errText, type ObjRef } from "./api";
import { restartable, scalable } from "./kinds";
import { aiEnabled, canExplain, explain } from "./ai";
import { guardProd, isProdLike, prodWarning, useStore, type DetailTab } from "./store";

export interface Action {
  id: string;
  label: string;
  shortcut?: string;
  danger?: boolean;
  run: () => void;
}

const st = () => useStore.getState();

async function attempt(label: string, fn: () => Promise<unknown>) {
  try {
    await fn();
    st().toast(label, "success");
  } catch (e) {
    st().toast(errText(e), "error");
  }
}

const describe = (r: ObjRef) => `${r.kind.kind.toLowerCase()}/${r.name}${r.namespace ? ` in ${r.namespace}` : ""}`;

export function actionsFor(ref: ObjRef): Action[] {
  const { kind } = ref;
  const open = (tab: DetailTab) => () => st().openDetail(ref, tab);
  const list: Action[] = [{ id: "details", label: "Details", shortcut: "↵", run: open("overview") }];
  const isPod = kind.group === "" && kind.kind === "Pod";

  if (isPod) {
    list.push({ id: "logs", label: "Logs", shortcut: "L", run: open("logs") });
    list.push({ id: "shell", label: "Shell", shortcut: "S", run: open("shell") });
  }
  if (kind.group === "" && kind.kind === "Node") list.push({ id: "shell", label: "Node shell", shortcut: "S", run: open("shell") });
  list.push({ id: "yaml", label: "Edit YAML", shortcut: "E", run: open("yaml") });
  list.push({ id: "events", label: "Events", run: open("events") });
  if (aiEnabled() && canExplain(ref)) list.push({ id: "explain", label: "Explain with AI", shortcut: "A", run: () => explain(ref) });

  if (scalable(kind)) {
    list.push({
      id: "scale",
      label: "Scale…",
      run: async () => {
        let current = "";
        try {
          current = String((await api.getObject(ref))?.spec?.replicas ?? "");
        } catch {
          /* ignore */
        }
        st().ask({
          title: `Scale ${describe(ref)}`,
          body: "Number of replicas",
          confirmLabel: "Scale",
          input: current,
          prod: prodWarning(ref.context),
          onConfirm: (v) => {
            const n = Number(v);
            if (!Number.isInteger(n) || n < 0) return st().toast("Replicas must be a non-negative integer", "error");
            return attempt(`Scaled ${ref.name} to ${n}`, () => api.scale(ref, n));
          },
        });
      },
    });
  }
  if (restartable(kind)) {
    list.push({
      id: "restart",
      label: "Rollout restart",
      shortcut: "R",
      run: () =>
        confirmIfNeeded(ref, `Restart ${describe(ref)}?`, "Pods will be replaced with a rolling update.", "Restart", false, false, () =>
          attempt(`Restarted ${ref.name}`, () => api.rolloutRestart(ref)),
        ),
    });
  }
  if (kind.group === "" && kind.kind === "Node") {
    const cordon = (on: boolean) => () =>
      guardProd(
        ref.context,
        `${on ? "Cordon" : "Uncordon"} node ${ref.name}?`,
        on ? "No new pods will be scheduled on this node." : "New pods can be scheduled on this node again.",
        on ? "Cordon" : "Uncordon",
        () => attempt(`${on ? "Cordoned" : "Uncordoned"} ${ref.name}`, () => api.setCordon(ref.context, ref.name, on)),
      );
    list.push({ id: "cordon", label: "Cordon", run: cordon(true) });
    list.push({ id: "uncordon", label: "Uncordon", run: cordon(false) });
  }
  if (kind.group === "batch" && kind.kind === "CronJob") {
    const suspend = (on: boolean) => () =>
      guardProd(
        ref.context,
        `${on ? "Suspend" : "Resume"} ${describe(ref)}?`,
        on ? "No new jobs will start until it is resumed." : "Jobs will start again on schedule.",
        on ? "Suspend" : "Resume",
        () => attempt(`${on ? "Suspended" : "Resumed"} ${ref.name}`, () => api.suspendCronjob(ref.context, ref.namespace!, ref.name, on)),
      );
    list.push({ id: "suspend", label: "Suspend", run: suspend(true) });
    list.push({ id: "resume", label: "Resume", run: suspend(false) });
  }
  list.push({
    id: "copy",
    label: "Copy name",
    shortcut: "C",
    run: () => {
      navigator.clipboard.writeText(ref.name);
      st().toast(`Copied ${ref.name}`);
    },
  });
  if (!kind.verbs || kind.verbs.length === 0 || kind.verbs.includes("delete")) {
    list.push({
      id: "delete",
      label: "Delete",
      shortcut: "⌫",
      danger: true,
      run: () =>
        confirmIfNeeded(ref, `Delete ${describe(ref)}?`, `Context: ${ref.context}. This cannot be undone.`, "Delete", true, true, () =>
          attempt(`Deleted ${ref.name}`, () => api.deleteObject(ref)),
        ),
    });
  }
  return list;
}

/** Destructive actions always ask. With `typeName`, production contexts also require typing the object's name,
 * even when confirmations for production are turned off. */
function confirmIfNeeded(ref: ObjRef, title: string, body: string, confirmLabel: string, danger: boolean, typeName: boolean, run: () => Promise<void>) {
  st().ask({
    title,
    body,
    confirmLabel,
    danger,
    typeToConfirm: typeName && isProdLike(ref.context) ? ref.name : undefined,
    prod: prodWarning(ref.context) ?? (typeName && isProdLike(ref.context) ? ref.context : undefined),
    onConfirm: run,
  });
}

export function runShortcut(ref: ObjRef, key: string): boolean {
  const map: Record<string, string> = { a: "explain", l: "logs", s: "shell", e: "yaml", y: "yaml", r: "restart", c: "copy", Delete: "delete", Backspace: "delete" };
  const id = map[key];
  if (!id) return false;
  const a = actionsFor(ref).find((x) => x.id === id);
  if (!a) return false;
  a.run();
  return true;
}

// ---------------------------------------------------------------- bulk actions (marked rows)

const plural = (n: number, ref: ObjRef) => `${n} ${n === 1 ? ref.kind.kind.toLowerCase() : ref.kind.plural}`;

/** Objects grouped by cluster for the confirmation dialog, clusters and names sorted. */
export function groupByCluster(refs: ObjRef[]): { group: string; items: string[] }[] {
  const by = new Map<string, string[]>();
  for (const r of refs) {
    const list = by.get(r.context) ?? [];
    list.push(r.namespace ? `${r.namespace}/${r.name}` : r.name);
    by.set(r.context, list);
  }
  return [...by.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([group, items]) => ({ group, items: items.sort() }));
}

/** One line for the result of a bulk action: "Deleted 12 pods" or "Deleted 11 of 12 pods · 1 failed: forbidden". */
export function bulkSummary(verb: string, refs: ObjRef[], errors: (string | null)[]): { text: string; ok: boolean } {
  const failed = errors.map((e, i) => [e, refs[i]] as const).filter(([e]) => e);
  if (failed.length === 0) return { text: `${verb} ${plural(refs.length, refs[0])}`, ok: true };
  const [first, ref] = failed[0];
  const done = refs.length - failed.length;
  return {
    text: `${verb} ${done} of ${plural(refs.length, refs[0])} · ${failed.length} failed (${ref.name}: ${first}${failed.length > 1 ? ", …" : ""})`,
    ok: false,
  };
}

/** After a bulk action, only the rows that failed stay marked, to retry or look at. */
function keepMarked(refs: ObjRef[], errors: (string | null)[]) {
  const failed = new Set(refs.filter((_, i) => errors[i]));
  const marked = new Map([...st().marked].filter(([, r]) => failed.has(r)));
  st().set({ marked });
}

const prodContexts = (refs: ObjRef[]) => [...new Set(refs.map((r) => r.context))].filter(isProdLike).sort();

export function bulkActionsFor(refs: ObjRef[]): Action[] {
  if (refs.length === 0) return [];
  const { kind } = refs[0];
  const what = plural(refs.length, refs[0]);
  const list: Action[] = [];
  if (restartable(kind)) {
    list.push({
      id: "restart",
      label: `Restart ${what}`,
      shortcut: "R",
      run: () => {
        const prod = prodContexts(refs).filter((c) => prodWarning(c));
        st().ask({
          title: `Restart ${what}?`,
          body: "Their pods will be replaced with a rolling update.",
          confirmLabel: "Restart",
          list: groupByCluster(refs),
          prod: prod.length ? prod.join(", ") : undefined,
          onConfirm: async () => {
            const errors = await Promise.all(refs.map((r) => api.rolloutRestart(r).then(() => null, (e) => errText(e))));
            const s = bulkSummary("Restarted", refs, errors);
            st().toast(s.text, s.ok ? "success" : "error");
            keepMarked(refs, errors);
          },
        });
      },
    });
  }
  list.push({
    id: "copy",
    label: "Copy names",
    shortcut: "C",
    run: () => {
      navigator.clipboard.writeText(refs.map((r) => r.name).join("\n"));
      st().toast(`Copied ${refs.length} names`);
    },
  });
  if (!kind.verbs || kind.verbs.length === 0 || kind.verbs.includes("delete")) {
    const isPod = kind.group === "" && kind.kind === "Pod";
    list.push({
      id: "delete",
      label: `Delete ${what}`,
      shortcut: "⌫",
      danger: true,
      run: () => {
        // Typing every name is impractical: on production the user types the count instead.
        const prod = prodContexts(refs);
        st().ask({
          title: `Delete ${what}?`,
          body: `This cannot be undone.${isPod ? " Pods run by a Deployment, StatefulSet, DaemonSet or Job are replaced by new ones." : ""}`,
          confirmLabel: "Delete",
          danger: true,
          list: groupByCluster(refs),
          typeToConfirm: prod.length ? `delete ${refs.length}` : undefined,
          prod: prod.length ? prod.join(", ") : undefined,
          check: isPod ? "Force: skip the grace period (for pods stuck in Terminating)" : undefined,
          onConfirm: async (_, force) => {
            let errors: (string | null)[];
            try {
              errors = await api.deleteObjects(kind, refs, force);
            } catch (e) {
              return st().toast(errText(e), "error");
            }
            const s = bulkSummary("Deleted", refs, errors);
            st().toast(s.text, s.ok ? "success" : "error");
            keepMarked(refs, errors);
          },
        });
      },
    });
  }
  return list;
}

export function runBulkShortcut(refs: ObjRef[], key: string): boolean {
  const map: Record<string, string> = { r: "restart", c: "copy", Delete: "delete", Backspace: "delete" };
  const a = bulkActionsFor(refs).find((x) => x.id === map[key]);
  if (!a) return false;
  a.run();
  return true;
}
