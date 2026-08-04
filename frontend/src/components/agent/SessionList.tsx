import { MessageSquare } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { AgentSession } from '../../types';

interface Props {
  sessions: AgentSession[];
  activeId: string;
  onSelect: (id: string) => void;
}

export default function SessionList({ sessions, activeId, onSelect }: Props) {
  return (
    <div className="flex-1 overflow-y-auto border-b border-border/60 p-2">
      <p className="px-2 py-1 text-xs font-medium uppercase tracking-wider text-muted-foreground">
        会话
      </p>
      {sessions.length === 0 ? (
        <p className="px-2 py-2 text-xs text-muted-foreground">暂无会话，点击上方「新会话」开始</p>
      ) : (
        <div className="space-y-1">
          {sessions.map((s) => (
            <button
              key={s.id}
              onClick={() => onSelect(s.id)}
              className={cn(
                'flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm transition-colors hover:bg-accent',
                s.id === activeId && 'bg-primary/10 text-primary'
              )}
            >
              <MessageSquare className="h-3.5 w-3.5 shrink-0" />
              <span className="truncate">{s.title || '未命名会话'}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
