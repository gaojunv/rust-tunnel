import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useLanguagePreference } from '@/i18n/I18nProvider';
import { LANGUAGE_PREFERENCES, type LanguagePreference } from '@/i18n/languagePreference';
import { Languages } from 'lucide-react';

/** Explicit key map for tsc type-safety with dynamic preference values. */
const PREFERENCE_LABEL_KEYS = {
  system: 'settings.appearance.system',
  'zh-CN': 'settings.appearance.zh-CN',
  en: 'settings.appearance.en',
} as const;

export default function AppearanceTab() {
  const { t } = useTranslation();
  const { preference, setPreference } = useLanguagePreference();

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-3">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
            <Languages className="h-4 w-4" />
          </div>
          <CardTitle className="text-lg">{t('settings.appearance.title')}</CardTitle>
        </div>
      </CardHeader>
      <CardContent>
        <div className="space-y-2">
          <label className="text-sm font-medium">{t('settings.appearance.language')}</label>
          <Select
            value={preference}
            onValueChange={(value) => setPreference(value as LanguagePreference)}
          >
            <SelectTrigger className="w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {LANGUAGE_PREFERENCES.map((p) => (
                <SelectItem key={p} value={p}>
                  {t(PREFERENCE_LABEL_KEYS[p])}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </CardContent>
    </Card>
  );
}
