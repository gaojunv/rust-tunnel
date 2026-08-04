import { useTranslation } from 'react-i18next';

export default function TerminalPanel() {
  const { t } = useTranslation();
  return <p className="text-xs text-muted-foreground">{t('agent.terminalComingSoon')}</p>;
}
