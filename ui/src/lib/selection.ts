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
