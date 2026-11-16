import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { Markdown } from "../markdown";

const html = (text: string) => renderToStaticMarkup(<Markdown text={text} />);

describe("Markdown", () => {
  it("renders headings, lists, code and emphasis", () => {
    const out = html("**The pod is OOMKilled.**\n\n## Why\n- uses `510Mi`\n- limit *512Mi*\n\n1. raise it\n2. check\n\n```yaml\nlimits:\n  memory: 1Gi\n```");
    expect(out).toContain("<p><strong>The pod is OOMKilled.</strong></p>");
    expect(out).toContain("<h4>Why</h4>");
    expect(out).toContain("<ul><li>uses <code>510Mi</code></li><li>limit <em>512Mi</em></li></ul>");
    expect(out).toContain('<ol start="1"><li>raise it</li><li>check</li></ol>');
    expect(out).toContain("<pre>limits:\n  memory: 1Gi</pre>");
  });

  it("never injects HTML", () => {
    expect(html("<img src=x onerror=alert(1)>")).toContain("&lt;img");
  });

  it("copes with half-streamed text", () => {
    expect(html("```bash\nkubectl get po")).toContain("<pre>kubectl get po</pre>");
    expect(html("a **bold")).toContain("a **bold");
  });

  it("keeps snake_case names intact", () => {
    expect(html("set max_connections and db_pool_size")).toContain("set max_connections and db_pool_size");
  });
});
