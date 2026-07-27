import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { useTranslation } from 'react-i18next';
import { usePreferences } from '../preferences/PreferencesProvider';
import {
  detectSystemLanguage,
  resolveLanguage,
  type LanguagePreference,
  type ResolvedLanguage,
} from './languagePreference';

interface LanguageContextValue {
  preference: LanguagePreference;
  resolvedLanguage: ResolvedLanguage;
  setPreference: (pref: LanguagePreference) => void;
}

const LanguageContext = createContext<LanguageContextValue | undefined>(undefined);

function getSystemLanguage(): ResolvedLanguage {
  if (typeof navigator === 'undefined') return 'en';
  return detectSystemLanguage(navigator.language);
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const { i18n } = useTranslation();
  const { prefs, setPreference: setGlobalPreference } = usePreferences();
  const [systemLanguage, setSystemLanguage] = useState<ResolvedLanguage>(() => getSystemLanguage());

  const preference = prefs.language;
  const resolvedLanguage = useMemo(
    () => resolveLanguage(preference, systemLanguage),
    [preference, systemLanguage],
  );

  useEffect(() => {
    if (i18n.language !== resolvedLanguage) {
      void i18n.changeLanguage(resolvedLanguage);
    }
  }, [i18n, resolvedLanguage]);

  useEffect(() => {
    const onLanguageChange = () => setSystemLanguage(getSystemLanguage());
    window.addEventListener('languagechange', onLanguageChange);
    return () => window.removeEventListener('languagechange', onLanguageChange);
  }, []);

  const setPreference = useCallback(
    (pref: LanguagePreference) => {
      setGlobalPreference('language', pref);
    },
    [setGlobalPreference],
  );

  const value = useMemo(
    () => ({ preference, resolvedLanguage, setPreference }),
    [preference, resolvedLanguage, setPreference],
  );

  return <LanguageContext.Provider value={value}>{children}</LanguageContext.Provider>;
}

export function useLanguagePreference(): LanguageContextValue {
  const ctx = useContext(LanguageContext);
  if (!ctx) throw new Error('useLanguagePreference must be used within I18nProvider');
  return ctx;
}
