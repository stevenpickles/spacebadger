// The color scheme the system asks for, shared by everything that draws in
// JavaScript (the treemap canvas, legend swatches) rather than through CSS.

const media = window.matchMedia("(prefers-color-scheme: dark)");

export const theme = $state({ dark: media.matches });

media.addEventListener("change", () => (theme.dark = media.matches));

/**
 * A CSS custom property from `:root`, e.g. `cssVar("--accent")`. Read it
 * where `theme.dark` is tracked so the value follows theme changes.
 */
export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}
