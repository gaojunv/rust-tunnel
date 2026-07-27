import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Info, Lock } from 'lucide-react';

export default function SecurityTab() {
  const { t } = useTranslation();

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Lock className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.security.title')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p>
                {t('settings.security.note')}
              </p>
              <p className="mt-2">
                {t('settings.security.changeNote')}
              </p>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
