export type ThemeMode = "light" | "dark" | "system";
export type AccentKey = "blue" | "green" | "teal" | "violet" | "amber" | "custom";

export const ACCENTS: Record<Exclude<AccentKey, "custom">, string> = {
  blue: "#0f7de0",
  green: "#16a34a",
  teal: "#0d9488",
  violet: "#7c3aed",
  amber: "#d97706",
};

const ACCENT_KEYS: Exclude<AccentKey, "custom">[] = ["blue", "green", "teal", "violet", "amber"];

function effectiveMode(mode: ThemeMode): "light" | "dark" {
  if (mode === "system") {
    return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return mode;
}

function isValidHex(hex: string): boolean {
  return /^#[0-9a-fA-F]{6}$/.test(hex);
}

export function applyTheme(mode: ThemeMode, accent: AccentKey | string) {
  const root = document.documentElement;
  root.dataset.theme = effectiveMode(mode);
  const accentValue = isValidHex(accent) ? accent : ACCENTS[accent as Exclude<AccentKey, "custom">] ?? ACCENTS.blue;
  root.style.setProperty("--accent", accentValue);
}

export { ACCENT_KEYS };
