import { useRouteError, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';

export default function ErrorPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const error = useRouteError() as Error | null;
  const message = error instanceof Error ? error.message : String(error ?? '');

  return (
    <div className="flex min-h-[60vh] flex-col items-center justify-center gap-4 px-4 py-16 text-center">
      <h1 className="text-2xl font-semibold">{t('error.title')}</h1>
      <p className="max-w-md text-sm text-muted-foreground">{t('error.description')}</p>
      {message && (
        <pre className="max-w-full overflow-auto rounded bg-muted px-3 py-2 text-left text-xs text-muted-foreground">
          {message}
        </pre>
      )}
      <div className="flex gap-2">
        <Button variant="outline" onClick={() => window.location.reload()}>
          {t('error.retry')}
        </Button>
        <Button onClick={() => navigate('/dashboard')}>{t('error.goHome')}</Button>
      </div>
    </div>
  );
}
