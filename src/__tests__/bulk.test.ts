import { describe, expect, it } from "vitest";
import { bulkSummary, groupByCluster } from "../actions";
import { PODS } from "../kinds";
import type { ObjRef } from "../api";

const pod = (context: string, namespace: string, name: string): ObjRef => ({ context, kind: PODS, namespace, name });

describe("groupByCluster", () => {
  it("groups by cluster, sorted, with namespace/name", () => {
    const refs = [pod("prod-us", "shop", "web-2"), pod("prod-eu", "shop", "web-1"), pod("prod-us", "data", "etl-1")];
    expect(groupByCluster(refs)).toEqual([
      { group: "prod-eu", items: ["shop/web-1"] },
      { group: "prod-us", items: ["data/etl-1", "shop/web-2"] },
    ]);
  });
});

describe("bulkSummary", () => {
  const refs = [pod("a", "ns", "p1"), pod("a", "ns", "p2"), pod("b", "ns", "p3")];

  it("says how many when all succeed", () => {
    expect(bulkSummary("Deleted", refs, [null, null, null])).toEqual({ text: "Deleted 3 pods", ok: true });
    expect(bulkSummary("Deleted", refs.slice(0, 1), [null])).toEqual({ text: "Deleted 1 pod", ok: true });
  });

  it("names the first failure", () => {
    const s = bulkSummary("Deleted", refs, [null, "forbidden", "forbidden"]);
    expect(s.ok).toBe(false);
    expect(s.text).toBe("Deleted 1 of 3 pods · 2 failed (p2: forbidden, …)");
  });
});
