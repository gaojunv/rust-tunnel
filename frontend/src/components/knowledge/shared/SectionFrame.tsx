import { Button } from '@/components/ui/button';
import { Plus, Settings } from 'lucide-react';

interface Props {
  title: string;
  count: number;
  newLabel: string;
  onNew: () => void;
  onSettings?: () => void;
  settingsLabel: string;
  children: React.ReactNode;
}

/** 分区统一头部：标题 + 计数 + 齿轮/新建按钮 + 内容区。 */
export default function SectionFrame({ title, count, newLabel, onNew, onSettings, settingsLabel, children }: Props) {
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {title} ({count})
        </h2>
        <div className="flex items-center gap-2">
          {onSettings && (
            <Button variant="outline" size="sm" onClick={onSettings} aria-label={settingsLabel}>
              <Settings className="h-4 w-4" />
            </Button>
          )}
          <Button size="sm" onClick={onNew}>
            <Plus className="mr-1 h-4 w-4" /> {newLabel}
          </Button>
        </div>
      </div>
      {children}
    </div>
  );
}
