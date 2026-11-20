export type ThemePreference = "light" | "dark" | "system";

interface ThemeController {
  subscribe(listener: (value: ThemePreference) => void): () => void;
  set(value: ThemePreference): void;
}

declare global {
  interface Window {
    oasisTheme: ThemeController;
  }
}

// The synchronous bootstrap owns persistence, OS changes, and cross-tab updates.
export const themePreference = window.oasisTheme;
