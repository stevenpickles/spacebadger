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
