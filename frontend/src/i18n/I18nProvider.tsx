import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import i18n from './index';
import {
  detectSystemLanguage,
  readStoredLanguagePreference,
  resolveLanguage,
  writeStoredLanguagePreference,
  type LanguagePreference,
  type ResolvedLanguage,
} from './languagePreference';

interface LanguageContextValue {
  preference: LanguagePreference;
  resolvedLanguage: ResolvedLanguage;
  setPreference: (preference: LanguagePreference) => void;
}

const LanguageContext = createContext<LanguageContextValue | undefined>(undefined);

const getStorage = (): Storage | undefined => {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
};

const getSystemLanguage = (): ResolvedLanguage => {
  if (typeof navigator === 'undefined') {
    return 'en';
  }

  return detectSystemLanguage(navigator.language);
};

interface I18nProviderProps {
  children: ReactNode;
}

export const I18nProvider = ({ children }: I18nProviderProps) => {
  const [preference, setPreferenceState] = useState<LanguagePreference>(() =>
    readStoredLanguagePreference(getStorage()),
  );
  const [systemLanguage, setSystemLanguage] = useState<ResolvedLanguage>(() =>
    getSystemLanguage(),
  );

  const resolvedLanguage = resolveLanguage(preference, systemLanguage);

  useEffect(() => {
    if (i18n.language !== resolvedLanguage) {
      void i18n.changeLanguage(resolvedLanguage);
    }
  }, [resolvedLanguage]);

  useEffect(() => {
    if (typeof window === 'undefined') {
      return undefined;
    }

    const handleChange = () => {
      setSystemLanguage(getSystemLanguage());
    };

    window.addEventListener('languagechange', handleChange);

    return () => {
      window.removeEventListener('languagechange', handleChange);
    };
  }, []);

  const setPreference = useCallback((nextPreference: LanguagePreference) => {
    setPreferenceState(nextPreference);
    writeStoredLanguagePreference(nextPreference, getStorage());
  }, []);

  const value = useMemo(
    () => ({ preference, resolvedLanguage, setPreference }),
    [preference, resolvedLanguage, setPreference],
  );

  return <LanguageContext.Provider value={value}>{children}</LanguageContext.Provider>;
};

export const useLanguagePreference = (): LanguageContextValue => {
  const context = useContext(LanguageContext);

  if (!context) {
    throw new Error('useLanguagePreference must be used within an I18nProvider');
  }

  return context;
};
