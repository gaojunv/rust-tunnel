import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useTheme } from '../../theme/ThemeProvider';
import type { ThemePreference } from '../../theme/theme';
import type { ReactNode } from 'react';

/** Explicit key map for tsc type-safety with dynamic theme preference values. */
const THEME_LABEL_KEYS = {
  system: 'theme.system',
  light: 'theme.light',
  dark: 'theme.dark',
} as const;

function getDescKey(value: ThemePreference): 'theme.systemDesc' | undefined {
  return value === 'system' ? 'theme.systemDesc' : undefined;
}

const SystemIcon = () => (
  <svg className="h-5 w-5" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
    <rect x="2" y="3" width="20" height="14" rx="2" />
    <path d="M8 21h8" />
    <path d="M12 17v4" />
  </svg>
);

const SunIcon = () => (
  <svg className="h-5 w-5" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
    <circle cx="12" cy="12" r="5" />
    <line x1="12" y1="1" x2="12" y2="3" />
    <line x1="12" y1="21" x2="12" y2="23" />
    <line x1="4.22" y1="4.22" x2="5.64" y2="5.64" />
    <line x1="18.36" y1="18.36" x2="19.78" y2="19.78" />
    <line x1="1" y1="12" x2="3" y2="12" />
    <line x1="21" y1="12" x2="23" y2="12" />
    <line x1="4.22" y1="19.78" x2="5.64" y2="18.36" />
    <line x1="18.36" y1="5.64" x2="19.78" y2="4.22" />
  </svg>
);

const MoonIcon = () => (
  <svg className="h-5 w-5" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
    <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
  </svg>
);

const CheckIcon = () => (
  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
    <polyline points="20 6 9 17 4 12" />
  </svg>
);

const iconByPreference: Record<ThemePreference, ReactNode> = {
  system: <SystemIcon />,
  light: <SunIcon />,
  dark: <MoonIcon />,
};

/** Ordered list of theme options for rendering. */
const THEME_OPTIONS: ThemePreference[] = ['system', 'light', 'dark'];

export const ThemeToggle = () => {
  const { t } = useTranslation();
  const { preference, setPreference } = useTheme();
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClickOutside = (event: PointerEvent) => {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setIsOpen(false);
      }
    };

    const handleEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setIsOpen(false);
      }
    };

    if (isOpen) {
      document.addEventListener('pointerdown', handleClickOutside);
      document.addEventListener('keydown', handleEscape);
    }

    return () => {
      document.removeEventListener('pointerdown', handleClickOutside);
      document.removeEventListener('keydown', handleEscape);
    };
  }, [isOpen]);

  const handleSelect = (value: ThemePreference) => {
    setPreference(value);
    setIsOpen(false);
  };

  return (
    <div className="relative" ref={containerRef}>
      <button
        onClick={() => setIsOpen(!isOpen)}
        aria-label={t('theme.ariaLabel')}
        aria-haspopup="menu"
        aria-expanded={isOpen}
        className="rounded-md p-2 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus:outline-none focus:ring-2 focus:ring-primary"
      >
        {iconByPreference[preference]}
      </button>

      {isOpen && (
        <div className="absolute right-0 z-50 mt-2 w-56 rounded-lg border bg-popover shadow-xl focus:outline-none">
          <div className="py-1" role="menu" aria-orientation="vertical">
            {THEME_OPTIONS.map((value) => {
              const labelKey = THEME_LABEL_KEYS[value];
              const descKey = getDescKey(value);
              return (
                <button
                  key={value}
                  onClick={() => handleSelect(value)}
                  className={`group flex items-center w-full px-4 py-2 text-sm text-left transition-colors ${
                    preference === value
                      ? 'bg-accent text-foreground'
                      : 'text-muted-foreground hover:bg-accent hover:text-foreground'
                  }`}
                  role="menuitemradio"
                  aria-checked={preference === value}
                >
                  <span className="mr-3 flex h-6 w-6 items-center justify-center text-muted-foreground">
                    {iconByPreference[value]}
                  </span>
                  <span className="flex-1">
                    <div className="font-medium">{t(labelKey)}</div>
                    {descKey && (
                      <div className="text-xs text-muted-foreground">{t(descKey)}</div>
                    )}
                  </span>
                  {preference === value && (
                    <span className="ml-3 text-primary">
                      <CheckIcon />
                    </span>
                  )}
                </button>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
};
