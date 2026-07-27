import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { fetchPreferences, updatePreferences } from '../api/preferences';
import {
  DEFAULT_USER_PREFERENCES,
  PREFERENCES_CACHE_KEY,
  readCachedPreferences,
  writeCachedPreferences,
  type UserPreferences,
} from './preferencesStore';

interface PreferencesContextValue {
  prefs: UserPreferences;
  setPreference: <K extends keyof UserPreferences>(key: K, value: UserPreferences[K]) => void;
  isSyncing: boolean;
}

const PreferencesContext = createContext<PreferencesContextValue | undefined>(undefined);

function getStorage(): Storage | undefined {
  try {
    return typeof window !== 'undefined' ? window.localStorage : undefined;
  } catch {
    return undefined;
  }
}

/** 后端用 snake_case，前端用 camelCase；这里做映射 */
function toApiShape(prefs: UserPreferences): {
  theme: string;
  language: string;
  title_effect: string;
} {
  return {
    theme: prefs.theme,
    language: prefs.language,
    title_effect: prefs.titleEffect,
  };
}

function fromApiShape(api: { theme: string; language: string; title_effect: string }): UserPreferences {
  return {
    theme: (api.theme as UserPreferences['theme']) ?? DEFAULT_USER_PREFERENCES.theme,
    language: (api.language as UserPreferences['language']) ?? DEFAULT_USER_PREFERENCES.language,
    titleEffect:
      (api.title_effect as UserPreferences['titleEffect']) ?? DEFAULT_USER_PREFERENCES.titleEffect,
  };
}

export function PreferencesProvider({ children }: { children: ReactNode }) {
  const [prefs, setPrefs] = useState<UserPreferences>(() => readCachedPreferences(getStorage()));
  const [isSyncing, setIsSyncing] = useState(false);

  // 启动：从服务器拉取并覆盖本地
  useEffect(() => {
    let cancelled = false;
    fetchPreferences()
      .then((apiPrefs) => {
        if (cancelled) return;
        const next = fromApiShape(apiPrefs);
        setPrefs(next);
        writeCachedPreferences(next, getStorage());
      })
      .catch(() => {
        // 网络失败：保留本地缓存
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // 跨标签页同步
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === PREFERENCES_CACHE_KEY) {
        setPrefs(readCachedPreferences(getStorage()));
      }
    };
    window.addEventListener('storage', onStorage);
    return () => window.removeEventListener('storage', onStorage);
  }, []);

  const setPreference = useCallback(
    <K extends keyof UserPreferences>(key: K, value: UserPreferences[K]) => {
      setPrefs((current) => {
        const next = { ...current, [key]: value };
        writeCachedPreferences(next, getStorage());

        setIsSyncing(true);
        updatePreferences(toApiShape(next))
          .catch(() => {
            // 回滚
            setPrefs(current);
            writeCachedPreferences(current, getStorage());
          })
          .finally(() => setIsSyncing(false));

        return next;
      });
    },
    [],
  );

  const value = useMemo(
    () => ({ prefs, setPreference, isSyncing }),
    [prefs, setPreference, isSyncing],
  );

  return <PreferencesContext.Provider value={value}>{children}</PreferencesContext.Provider>;
}

export function usePreferences(): PreferencesContextValue {
  const ctx = useContext(PreferencesContext);
  if (!ctx) {
    throw new Error('usePreferences must be used within PreferencesProvider');
  }
  return ctx;
}
