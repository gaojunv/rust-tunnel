import { useEffect, useRef, useState } from 'react';
import { useTheme } from '../../theme/ThemeProvider';
import type { ThemePreference } from '../../theme/theme';

interface ThemeOption {
  value: ThemePreference;
  label: string;
  description?: string;
}

const themeOptions: ThemeOption[] = [
  { value: 'system', label: '跟随系统', description: '根据系统主题自动切换' },
  { value: 'light', label: '浅色' },
  { value: 'dark', label: '深色' },
];

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

const getIcon = (preference: ThemePreference) => {
  switch (preference) {
    case 'system':
      return <SystemIcon />;
    case 'light':
      return <SunIcon />;
    case 'dark':
      return <MoonIcon />;
  }
};

export const ThemeToggle = () => {
  const { preference, setPreference } = useTheme();
  const [isOpen, setIsOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
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
      document.addEventListener('mousedown', handleClickOutside);
      document.addEventListener('keydown', handleEscape);
    }

    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
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
        aria-label="切换主题"
        aria-haspopup="menu"
        aria-expanded={isOpen}
        className="p-2 rounded-md text-gray-300 hover:text-white hover:bg-gray-700 dark:text-slate-200 dark:hover:bg-slate-700 focus:outline-none focus:ring-2 focus:ring-offset-2 focus:ring-offset-gray-800 focus:ring-white dark:focus:ring-offset-slate-900"
      >
        {getIcon(preference)}
      </button>

      {isOpen && (
        <div className="absolute right-0 mt-2 w-56 rounded-md shadow-lg bg-white dark:bg-slate-800 ring-1 ring-black ring-opacity-5 dark:ring-slate-700 focus:outline-none z-50">
          <div className="py-1" role="menu" aria-orientation="vertical">
            {themeOptions.map((option) => (
              <button
                key={option.value}
                onClick={() => handleSelect(option.value)}
                className={`group flex items-center w-full px-4 py-2 text-sm text-left ${
                  preference === option.value
                    ? 'bg-gray-100 dark:bg-slate-700 text-gray-900 dark:text-white'
                    : 'text-gray-700 dark:text-slate-200 hover:bg-gray-50 dark:hover:bg-slate-700'
                }`}
                role="menuitemradio"
                aria-checked={preference === option.value}
              >
                <span className="flex items-center justify-center w-6 h-6 mr-3 text-gray-400 dark:text-slate-400">
                  {getIcon(option.value)}
                </span>
                <span className="flex-1">
                  <div className="font-medium">{option.label}</div>
                  {option.description && (
                    <div className="text-xs text-gray-500 dark:text-slate-400">{option.description}</div>
                  )}
                </span>
                {preference === option.value && (
                  <span className="ml-3 text-gray-600 dark:text-slate-300">
                    <CheckIcon />
                  </span>
                )}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
};
