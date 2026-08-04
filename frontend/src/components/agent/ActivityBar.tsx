import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Folder, TerminalSquare, GitBranch } from 'lucide-react';
import { cn } from '@/lib/utils';
import FilesPanel from './panels/FilesPanel';
import TerminalPanel from './panels/TerminalPanel';
import GitPanel from './panels/GitPanel';

type PanelKind = 'files' | 'terminal' | 'git';

const ICONS: {
  kind: PanelKind;
  Icon: typeof Folder;
  labelKey: 'agent.files' | 'agent.terminal' | 'agent.git';
}[] = [
  { kind: 'files', Icon: Folder, labelKey: 'agent.files' },
  { kind: 'terminal', Icon: TerminalSquare, labelKey: 'agent.terminal' },
  { kind: 'git', Icon: GitBranch, labelKey: 'agent.git' },
];

export default function ActivityBar({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const [active, setActive] = useState<PanelKind | null>(null);

  const toggle = (kind: PanelKind) => setActive((cur) => (cur === kind ? null : kind));

  return (
    <div className="flex h-full shrink-0">
      {/* 极窄图标栏（VS Code Activity Bar） */}
      <div className="flex w-12 flex-col items-center gap-1 border-r border-border/60 py-2">
        {ICONS.map(({ kind, Icon, labelKey }) => (
          <button
            key={kind}
            type="button"
            aria-label={t(labelKey)}
            title={t(labelKey)}
            onClick={() => toggle(kind)}
            className={cn(
              'flex h-9 w-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground',
              active === kind && 'bg-accent text-primary'
            )}
          >
            <Icon className="h-4 w-4" />
          </button>
        ))}
      </div>

      {/* 可展开面板 */}
      {active && (
        <div
          data-testid="activity-panel"
          data-panel={active}
          className="w-72 overflow-y-auto border-r border-border/60 p-2"
        >
          {active === 'files' && <FilesPanel />}
          {active === 'terminal' && <TerminalPanel />}
          {active === 'git' && <GitPanel sessionId={sessionId} />}
        </div>
      )}
    </div>
  );
}
