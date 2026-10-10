import { KIND_OTHER, type DecodedLayout } from "./layoutWire";

/**
 * A selected treemap item, by stable node ID. `other` marks the merged
 * "other small items" region of folder `node`.
 */
export interface Selection {
  node: number;
  other: boolean;
}

export function sameSelection(a: Selection | null, b: Selection | null): boolean {
  return a === b || (!!a && !!b && a.node === b.node && a.other === b.other);
}

export type ItemKind = "file" | "folder" | "other";

/** Where a context menu was asked for, and for what. */
export interface MenuRequest {
  /** The item under the pointer or the selected item; `null` for empty space. */
  selection: Selection | null;
  kind: ItemKind | null;
  /** Window (client) coordinates. */
  x: number;
  y: number;
}

/**
 * A change to the selected items, as file managers do it: a plain click
 * selects one item, Ctrl (⌘ on macOS) adds or removes one, Shift selects
 * the range from the anchor, and Ctrl+Shift adds that range.
 */
export interface Pick {
  /** The item clicked or moved to; it gets the focus (and the details). */
  focus: Selection;
  /** Items the change applies to. Merged small-items regions are never items. */
  nodes: number[];
  mode: "replace" | "toggle" | "add";
  /** Shift ranges keep the anchor where it was. */
  keepAnchor: boolean;
}

interface Modifiers {
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
}

/** Ctrl on Windows and Linux, ⌘ on macOS. */
export function toggles(e: Modifiers): boolean {
  return e.ctrlKey || e.metaKey;
}

/**
 * The pick a click on `focus` makes. `range` lists the items from the
 * anchor to `focus` when Shift is held and a range exists, else `null`.
 */
export function clickPick(focus: Selection, e: Modifiers, range: number[] | null): Pick {
  const own = focus.other ? [] : [focus.node];
  if (e.shiftKey && range) {
    return { focus, nodes: range, mode: toggles(e) ? "add" : "replace", keepAnchor: true };
  }
  if (toggles(e)) return { focus, nodes: own, mode: "toggle", keepAnchor: false };
  return { focus, nodes: own, mode: "replace", keepAnchor: false };
}

/** The selected items after `pick`. */
export function applyPick(selected: number[], pick: Pick): number[] {
  if (pick.mode === "replace") return [...new Set(pick.nodes)];
  const next = new Set(selected);
  for (const n of pick.nodes) {
    if (pick.mode === "toggle" && next.has(n)) next.delete(n);
    else next.add(n);
  }
  return [...next];
}

/**
 * Items between the anchor and rectangle `to` in drawing order (largest
 * first), when both are in the same folder; otherwise `null`.
 */
export function siblingRange(l: DecodedLayout, anchor: number | null, to: number): number[] | null {
  if (anchor === null || l.kind[to] === KIND_OTHER) return null;
  const parent = l.parent[to];
  let from = -1;
  for (let i = 0; i < l.count; i++) {
    if (l.node[i] === anchor && l.kind[i] !== KIND_OTHER && l.parent[i] === parent) {
      from = i;
      break;
    }
  }
  if (from < 0) return null;
  const nodes: number[] = [];
  for (let i = Math.min(from, to); i <= Math.max(from, to); i++) {
    if (l.kind[i] !== KIND_OTHER && l.parent[i] === parent) nodes.push(l.node[i]!);
  }
  return nodes;
}

/** Items of a list from index `from` to `to`, inclusive, in list order. */
export function listRange<T>(items: T[], from: number, to: number, node: (item: T) => number): number[] {
  const lo = Math.max(0, Math.min(from, to));
  const hi = Math.min(items.length - 1, Math.max(from, to));
  return items.slice(lo, hi + 1).map(node);
}
