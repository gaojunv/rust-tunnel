import { useTranslation } from 'react-i18next';

export default function FilesPanel() {
  const { t } = useTranslation();
  return <p className="text-xs text-muted-foreground">{t('agent.filesComingSoon')}</p>;
}
