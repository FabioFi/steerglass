import type { ReactNode } from "react";

/** A small Markdown renderer for AI answers: headings, paragraphs, lists, code blocks, bold,
 * italics and inline code. It builds React elements (no HTML injection) and copes with the
 * half-written text of an answer that is still streaming. */
export function Markdown({ text }: { text: string }) {
  return <>{blocks(text)}</>;
}

function blocks(text: string): ReactNode[] {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const out: ReactNode[] = [];
  let i = 0;
  let key = 0;
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^\s*(```|~~~)\s*([\w+-]*)/);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i++]);
      i++;
      out.push(<CodeBlock key={key++} code={body.join("\n")} />);
      continue;
    }
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      const level = Math.min(h[1].length + 2, 6);
      const Tag = `h${level}` as "h3";
      out.push(<Tag key={key++}>{inline(h[2])}</Tag>);
      i++;
      continue;
    }
    if (/^\s*([-*+]|\d+[.)])\s+/.test(line)) {
      const ordered = /^\s*\d+[.)]/.test(line);
      const items: string[] = [];
      const start = Number(line.match(/^\s*(\d+)/)?.[1] ?? 1);
      const sameKind = (l: string) => (ordered ? /^\d+[.)]\s+/ : /^[-*+]\s+/).test(l);
      while (i < lines.length && lines[i].trim() !== "" && !/^\s*(```|~~~)/.test(lines[i])) {
        const m = lines[i].match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
        const nested = /^\s{2,}/.test(lines[i]);
        // A new item at the list's level, the end of the list (another kind of list), or the
        // continuation of the previous item.
        if (m && !nested && sameKind(lines[i].trimStart())) items.push(m[2]);
        else if (m && !nested && items.length) break;
        else if (items.length) items[items.length - 1] += "\n" + lines[i].replace(/^\s+/, "");
        else items.push(lines[i]);
        i++;
        // A blank line followed by another item of the same list keeps it going.
        if (lines[i]?.trim() === "" && sameKind(lines[i + 1] ?? "")) i++;
      }
      const children = items.map((it, n) => <li key={n}>{inline(it)}</li>);
      out.push(ordered ? <ol key={key++} start={start}>{children}</ol> : <ul key={key++}>{children}</ul>);
      continue;
    }
    if (line.trim() === "" || /^\s*(-{3,}|\*{3,})\s*$/.test(line)) {
      i++;
      continue;
    }
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() !== "" && !/^(#{1,6}\s|\s*(```|~~~)|\s*([-*+]|\d+[.)])\s+)/.test(lines[i])) para.push(lines[i++]);
    out.push(<p key={key++}>{inline(para.join("\n"))}</p>);
  }
  return out;
}

function CodeBlock({ code }: { code: string }) {
  return (
    <div className="md-code">
      <button className="md-copy" onClick={() => navigator.clipboard.writeText(code)} title="Copy">
        Copy
      </button>
      <pre>{code}</pre>
    </div>
  );
}

/** `code`, **bold**, *italic* / _italic_; line breaks inside a paragraph are kept. */
function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  const re = /(`+)([^`]+?)\1|\*\*(.+?)\*\*|__(.+?)__|\*(?!\s)(.+?)\*|(?<![\w])_(?!\s)(.+?)_(?![\w])|\n/g;
  let last = 0;
  let key = 0;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m.index > last) out.push(text.slice(last, m.index));
    if (m[0] === "\n") out.push(<br key={key++} />);
    else if (m[2] !== undefined) out.push(<code key={key++}>{m[2]}</code>);
    else if (m[3] !== undefined || m[4] !== undefined) out.push(<strong key={key++}>{inline(m[3] ?? m[4])}</strong>);
    else out.push(<em key={key++}>{inline(m[5] ?? m[6])}</em>);
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}
