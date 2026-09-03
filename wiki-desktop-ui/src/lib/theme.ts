import { useCallback, useState } from "react";

export type Theme = "dark" | "light";

const STORAGE_KEY = "wiki-theme";

function isTheme(value: unknown): value is Theme {
  return value === "dark" || value === "light";
}

export function getTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (isTheme(stored)) return stored;
  } catch {
    // ignore storage errors (e.g. private mode)
  }
  return "dark";
}

export function applyTheme(theme: Theme): void {
  const el = document.documentElement;
  if (theme === "light") {
    el.classList.add("light");
    el.classList.remove("dark");
  } else {
    el.classList.add("dark");
    el.classList.remove("light");
  }
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // ignore
  }
}

export function toggleTheme(): Theme {
  const next: Theme = getTheme() === "dark" ? "light" : "dark";
  applyTheme(next);
  return next;
}

export function useTheme(): { theme: Theme; toggleTheme: () => void } {
  const [theme, setTheme] = useState<Theme>(() => getTheme());

  const handleToggle = useCallback(() => {
    const next: Theme = theme === "dark" ? "light" : "dark";
    applyTheme(next);
    setTheme(next);
  }, [theme]);

  return { theme, toggleTheme: handleToggle };
}
