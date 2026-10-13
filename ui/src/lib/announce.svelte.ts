// One polite live region for the whole app (rendered by Announcer.svelte).
// Announce finished things only: a scan ending, a search total, the item a
// keyboard user moved to. Never counters that change while scanning.

export const announcer = $state({ text: "" });

let timer: ReturnType<typeof setTimeout> | undefined;

/** Says `text` once. Repeating the same text announces it again. */
export function announce(text: string) {
  // Clearing first makes screen readers read a repeated message.
  announcer.text = "";
  clearTimeout(timer);
  timer = setTimeout(() => (announcer.text = text), 50);
}
