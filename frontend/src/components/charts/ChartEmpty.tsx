import { useTranslation } from 'react-i18next';

interface ChartEmptyProps {
  message?: string;
  loading?: boolean;
}

export const ChartEmpty = ({ message, loading = false }: ChartEmptyProps) => {
  const { t } = useTranslation();
  const displayMessage = message ?? t('common.noData');
  return (
    <div className="flex h-[200px] w-full items-center justify-center text-sm text-muted-foreground">
      {loading ? t('common.loading') : displayMessage}
    </div>
  );
};
