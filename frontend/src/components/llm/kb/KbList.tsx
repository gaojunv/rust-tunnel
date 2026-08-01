import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Plus, BookOpen } from 'lucide-react';
import type { LlmKnowledgeBase } from '@/types';

interface Props {
  kbs: LlmKnowledgeBase[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
}

export default function KbList({ kbs, selectedId, onSelect, onNew }: Props) {
  const { t } = useTranslation();

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {t('kb.listTitle')} ({kbs.length})
        </h2>
        <Button size="sm" onClick={onNew}>
          <Plus className="mr-1 h-4 w-4" /> {t('kb.newKb')}
        </Button>
      </div>
      {kbs.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t('kb.empty')}
          </CardContent>
        </Card>
      ) : (
        kbs.map((kb) => (
          <Card
            key={kb.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === kb.id && 'border-primary/60 bg-primary/5'
            )}
            onClick={() => onSelect(kb.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="flex min-w-0 items-center gap-2">
                  <BookOpen className="h-4 w-4 shrink-0 text-muted-foreground" />
                  <span className="truncate font-medium">{kb.name}</span>
                </div>
                <Badge variant={kb.enabled ? 'default' : 'secondary'}>
                  {kb.enabled ? t('kb.enabled') : t('kb.disabled')}
                </Badge>
              </div>
              {kb.description && (
                <p className="mt-1 line-clamp-2 text-xs text-muted-foreground">{kb.description}</p>
              )}
              <div className="mt-2 text-xs text-muted-foreground">
                {t('kb.docCount', { count: kb.doc_count ?? 0 })} · {kb.emb_model}
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}
