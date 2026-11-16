import { useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Bug, Play, Server, Sparkles, X } from "lucide-react";
import { api, errText, type ExecMsg, type ObjRef } from "../api";
import { isDark, onThemeChange } from "../theme";
import { guardProd } from "../store";
import { aiBlocked, useAi } from "../ai";

const termTheme = () =>
  isDark()
    ? { background: "#0b0e15", foreground: "#e8ebf2", cursor: "#2dd4bf", selectionBackground: "#2a3243" }
    : { background: "#ffffff", foreground: "#1b1f27", cursor: "#0f766e", selectionBackground: "#cfe8e4" };

/** Images offered for debug containers; any other can be typed. */
const DEBUG_IMAGES = ["busybox:1.37", "nicolaka/netshoot:latest", "alpine:3.22", "ubuntu:24.04"];
const IMAGE_KEY = "steerglass.debugImage";

const savedImage = () => {
  try {
    return localStorage.getItem(IMAGE_KEY) || DEBUG_IMAGES[0];
  } catch {
    return DEBUG_IMAGES[0];
  }
};

/** The container image has no /bin/sh (distroless, scratch): exec can't start a shell. */
const noShell = (msg: string) => /\/bin\/sh/.test(msg) && /no such file|not found|executable file/i.test(msg);

type Mode = { kind: "shell" } | { kind: "debug"; image: string };
type Starter = (size: { cols: number; rows: number }, onMsg: (m: ExecMsg) => void) => Promise<number>;

/**
 * An xterm bound to one interactive session. `start` opens it (null: no session yet); after it closes,
 * Enter starts it again. `onFailure` may claim an error (returning true) to show it its own way.
 */
function useSession(
  host: React.RefObject<HTMLDivElement | null>,
  start: Starter | null,
  deps: unknown[],
  opts: { connecting?: string; again: string; onFailure?: (msg: string, term: XTerm) => boolean },
) {
  const [session, setSession] = useState(0);
  const live = useRef<{ term: XTerm; id: () => number | null } | null>(null);
  useEffect(() => {
    if (!start) return;
    const term = new XTerm({
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      fontSize: 12.5,
      cursorBlink: true,
      theme: termTheme(),
      scrollback: 10000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    fit.fit();
    term.focus();
    if (opts.connecting) term.writeln(`\x1b[2m${opts.connecting}\x1b[0m`);

    let id: number | null = null;
    let disposed = false;
    const failed = (msg: string) => {
      if (!opts.onFailure?.(msg, term)) term.writeln(`\r\n\x1b[31m${msg}\x1b[0m\r\n\x1b[2m— ${opts.again}\x1b[0m`);
    };
    start({ cols: term.cols, rows: term.rows }, (m) => {
      if (m.type === "data") term.write(new Uint8Array(m.data));
      else {
        id = null;
        if (m.message && opts.onFailure?.(m.message, term)) return;
        term.writeln(`\r\n\x1b[2m[session closed${m.message ? `: ${m.message}` : ""}] — ${opts.again}\x1b[0m`);
      }
    })
      .then((i) => {
        if (disposed) api.stopStream(i);
        else id = i;
      })
      .catch((e) => failed(errText(e)));

    const sub = term.onData((d) => {
      if (id !== null) api.execInput(id, d).catch(() => {});
      else if (d === "\r") setSession((s) => s + 1);
    });
    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
        if (id !== null) api.execResize(id, term.cols, term.rows);
      } catch {
        /* element hidden */
      }
    });
    ro.observe(host.current!);
    const offTheme = onThemeChange(() => (term.options.theme = termTheme()));
    live.current = { term, id: () => id };

    return () => {
      live.current = null;
      offTheme();
      disposed = true;
      ro.disconnect();
      sub.dispose();
      if (id !== null) api.stopStream(id);
      term.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, session]);
  return {
    restart: () => setSession((s) => s + 1),
    /** Types text at the prompt (not run: the user presses Enter). False without a session. */
    type: (text: string) => {
      const l = live.current;
      const id = l?.id();
      if (!l || id === null || id === undefined) return false;
      api.execInput(id, text).catch(() => {});
      l.term.focus();
      return true;
    },
    /** The last lines of the screen, as text. */
    screen: (lines = 40) => {
      const buf = live.current?.term.buffer.active;
      if (!buf) return "";
      const out: string[] = [];
      for (let i = Math.max(0, buf.length - 200); i < buf.length; i++) out.push(buf.getLine(i)?.translateToString(true) ?? "");
      return out.join("\n").trimEnd().split("\n").slice(-lines).join("\n");
    },
  };
}

/** "Describe a command": the AI writes it at the prompt, the user reads it and presses Enter. */
function AiCommandBar({ context, target, session, onClose }: { context: string; target: { pod?: string; container?: string; image?: string; node?: string }; session: ReturnType<typeof useSession>; onClose: () => void }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const blocked = aiBlocked(context);
  const go = async () => {
    if (!text.trim() || busy) return;
    setBusy(true);
    setNote(null);
    try {
      const { command } = await api.aiCommand(text.trim(), { ...target, screen: session.screen() });
      if (!command) setNote("The AI didn't suggest a command.");
      else if (command.startsWith("#")) setNote(command.replace(/^#\s*/, ""));
      else if (session.type(command)) {
        setText("");
        setNote("Typed at the prompt: read it, then press Enter in the terminal to run it.");
      } else setNote(`No session: ${command}`);
    } catch (e) {
      setNote(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="ai-cmd">
      <Sparkles size={13} className="ai-icon" />
      {blocked ? (
        <span className="muted small">{blocked}</span>
      ) : (
        <input
          className="text-input"
          autoFocus
          value={text}
          disabled={busy}
          placeholder={busy ? "Writing the command…" : "Describe what to do, e.g. the 10 biggest files under /var/log"}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") go();
            if (e.key === "Escape") onClose();
          }}
          spellCheck={false}
          aria-label="Describe a command"
        />
      )}
      {note && <span className="muted small ai-cmd-note">{note}</span>}
      <button className="icon-btn" onClick={onClose} title="Close">
        <X size={13} />
      </button>
    </div>
  );
}

/** The ✨ button that opens the command bar, when AI is on. */
function AiCommandButton({ open, onClick }: { open: boolean; onClick: () => void }) {
  const on = useAi((s) => !!s.settings?.enabled);
  if (!on) return null;
  return (
    <button className={open ? "btn small on" : "btn small"} onClick={onClick} title="Describe a command and let AI write it (you run it)">
      <Sparkles size={12} /> Ask for a command
    </button>
  );
}

/** The image for a debug container or node shell: a few presets, or any image typed in. */
function ImagePicker({
  title,
  children,
  warn,
  startLabel = "Start",
  onStart,
  onClose,
}: {
  title: string;
  children: React.ReactNode;
  warn?: boolean;
  startLabel?: string;
  onStart: (image: string) => void;
  onClose?: () => void;
}) {
  const [image, setImage] = useState(savedImage);
  const [custom, setCustom] = useState(() => !DEBUG_IMAGES.includes(image));
  const go = () => {
    const img = image.trim();
    if (!img) return;
    try {
      localStorage.setItem(IMAGE_KEY, img);
    } catch {
      /* not remembered */
    }
    onStart(img);
  };
  return (
    <div className={warn ? "debug-picker warn" : "debug-picker"}>
      <div className="debug-picker-text">
        <b>{title}</b>
        <span className="muted small">{children}</span>
      </div>
      <div className="row-gap">
        <select
          value={custom ? "" : image}
          onChange={(e) => {
            setCustom(!e.target.value);
            setImage(e.target.value);
          }}
        >
          {DEBUG_IMAGES.map((i) => (
            <option key={i} value={i}>
              {i}
            </option>
          ))}
          <option value="">Custom image…</option>
        </select>
        {custom && (
          <input
            className="mono"
            value={image}
            onChange={(e) => setImage(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && go()}
            placeholder="registry/image:tag"
            spellCheck={false}
            autoFocus
          />
        )}
        <button className="btn small primary" disabled={!image.trim()} onClick={go}>
          <Play size={12} /> {startLabel}
        </button>
        {onClose && (
          <button className="icon-btn" onClick={onClose} title="Close">
            <X size={13} />
          </button>
        )}
      </div>
    </div>
  );
}

export function Terminal({ objRef, initialContainer }: { objRef: ObjRef; initialContainer?: string }) {
  return objRef.kind.group === "" && objRef.kind.kind === "Node" ? <NodeTerminal objRef={objRef} /> : <PodTerminal objRef={objRef} initialContainer={initialContainer} />;
}

function PodTerminal({ objRef, initialContainer }: { objRef: ObjRef; initialContainer?: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [containers, setContainers] = useState<string[]>([]);
  const [container, setContainer] = useState<string | undefined>(initialContainer);
  const [mode, setMode] = useState<Mode>({ kind: "shell" });
  const [picker, setPicker] = useState<"closed" | "open" | "noShell">("closed");
  const [aiBar, setAiBar] = useState(false);
  const [images, setImages] = useState<Record<string, string>>({});

  const pick = (c: string) => {
    setMode({ kind: "shell" });
    setPicker("closed");
    setContainer(c);
  };

  useEffect(() => {
    api.getObject(objRef).then((o) => {
      setContainers((o.spec?.containers ?? []).map((c: any) => c.name));
      setImages(Object.fromEntries((o.spec?.containers ?? []).map((c: any) => [c.name, c.image])));
      setContainer((c) => c ?? o.spec?.containers?.[0]?.name);
    });
  }, [objRef]);

  const pod = { context: objRef.context, namespace: objRef.namespace!, pod: objRef.name };
  const session = useSession(
    host,
    !container
      ? null
      : mode.kind === "debug"
        ? (size, onMsg) => api.debug({ ...pod, ...size, target: container, image: mode.image }, onMsg)
        : (size, onMsg) => api.exec({ ...pod, ...size, container }, onMsg),
    [objRef, container, mode],
    {
      connecting: mode.kind === "shell" ? `Connecting to ${objRef.name}/${container}…` : undefined,
      again: mode.kind === "debug" ? "press Enter to start a new debug container" : "press Enter to reconnect",
      onFailure: (msg, term) => {
        if (mode.kind !== "shell" || !noShell(msg)) return false;
        term.writeln(`\x1b[33mThis container has no shell (/bin/sh is missing). Start a debug container to get one.\x1b[0m`);
        setPicker("noShell");
        return true;
      },
    },
  );
  const startDebug = (image: string) =>
    guardProd(
      objRef.context,
      `Start a debug container in ${objRef.name}?`,
      `A container with ${image} is added to the pod and stays there until the pod is replaced.`,
      "Start",
      () => {
        setPicker("closed");
        setMode({ kind: "debug", image });
        session.restart();
      },
    );

  return (
    <div className="terminal-tab">
      <div className="subbar">
        {containers.length > 1 && (
          <select value={container} onChange={(e) => pick(e.target.value)}>
            {containers.map((c) => (
              <option key={c}>{c}</option>
            ))}
          </select>
        )}
        {mode.kind === "debug" && (
          <span className="pill">
            <Bug size={11} /> debug container · {mode.image}
          </span>
        )}
        <span className="spacer" />
        {mode.kind === "debug" && (
          <button className="btn small" onClick={() => pick(container!)} title="Back to a shell in the container itself">
            Container shell
          </button>
        )}
        <button
          className={picker === "closed" ? "btn small" : "btn small on"}
          onClick={() => setPicker((p) => (p === "closed" ? "open" : "closed"))}
          title="Start an ephemeral debug container in this pod, like kubectl debug"
        >
          <Bug size={12} /> Debug container
        </button>
        <AiCommandButton open={aiBar} onClick={() => setAiBar((o) => !o)} />
      </div>
      {aiBar && (
        <AiCommandBar
          context={objRef.context}
          target={{ pod: objRef.name, container, image: mode.kind === "debug" ? mode.image : images[container ?? ""] }}
          session={session}
          onClose={() => setAiBar(false)}
        />
      )}
      {picker !== "closed" && (
        <ImagePicker
          title={picker === "noShell" ? "No shell in this container" : "Debug container"}
          warn={picker === "noShell"}
          onStart={startDebug}
          onClose={() => setPicker("closed")}
        >
          Adds a temporary container with the image you choose to the pod{container ? `, sharing the processes of ${container}` : ""}. It stays in the pod
          until the pod is replaced, and it needs permission to update pods/ephemeralcontainers.
        </ImagePicker>
      )}
      <div className="xterm-host" ref={host} />
    </div>
  );
}

/** A shell on the node itself (`kubectl debug node`): a privileged pod there, removed when the session closes. */
function NodeTerminal({ objRef }: { objRef: ObjRef }) {
  const host = useRef<HTMLDivElement>(null);
  const [image, setImage] = useState<string | null>(null);
  const [aiBar, setAiBar] = useState(false);
  const session = useSession(
    host,
    image ? (size, onMsg) => api.nodeShell({ context: objRef.context, node: objRef.name, image, ...size }, onMsg) : null,
    [objRef, image],
    { again: "press Enter to start a new node shell" },
  );
  return (
    <div className="terminal-tab">
      {image ? (
        <div className="subbar">
          <span className="pill">
            <Server size={11} /> node shell · {image}
          </span>
          <span className="spacer" />
          <button className="btn small" onClick={() => setImage(null)} title="Choose another image">
            Change image
          </button>
          <AiCommandButton open={aiBar} onClick={() => setAiBar((o) => !o)} />
        </div>
      ) : null}
      {image && aiBar && <AiCommandBar context={objRef.context} target={{ node: objRef.name }} session={session} onClose={() => setAiBar(false)} />}
      {!image && (
        <ImagePicker
          title={`Shell on node ${objRef.name}`}
          startLabel="Start node shell"
          onStart={(img) =>
            guardProd(
              objRef.context,
              `Open a root shell on node ${objRef.name}?`,
              `A privileged pod with ${img} is started on the node and removed when the session closes.`,
              "Start",
              () => {
                setImage(img);
                session.restart();
              },
            )
          }
        >
          Starts a privileged pod on this node, in namespace default, that enters the host: its processes, network and files, as root. The pod is removed when
          the session closes. It needs permission to create privileged pods there; the image needs nsenter (busybox, netshoot and ubuntu have it).
        </ImagePicker>
      )}
      <div className="xterm-host" ref={host} />
    </div>
  );
}
