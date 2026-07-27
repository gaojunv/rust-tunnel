import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import en from './locales/en/common.json';
import zhCN from './locales/zh-CN/common.json';

export const defaultNS = 'common';

export const resources = {
  en: { common: en },
  'zh-CN': { common: zhCN },
} as const;

i18n.use(initReactI18next).init({
  resources,
  defaultNS,
  fallbackLng: 'en',
  lng: 'en',
  interpolation: {
    escapeValue: false, // React 已转义
  },
  returnNull: false,
});

export default i18n;
