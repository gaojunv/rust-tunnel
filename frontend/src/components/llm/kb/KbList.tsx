import { useState, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Search } from 'lucide-react';
import SectionFrame from '@/components/knowledge/shared/SectionFrame';
import { useDebouncedSearch } from '@/components/knowledge/shared/useDebouncedSearch';
import type { LlmKnowledgeBase } from '@/types';

interface Props {
  kbs: LlmKnowledgeBase[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onSettings?: () => void;
}

export default function KbList({ kbs, selectedId, onSelect, onNew, onSettings }: Props) {
  const { t } = useTranslation();
  const [q, setQ] = useState('');
  const [qInput, setQInput] = useDebouncedSearch(q, setQ);
  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return kbs;
    return kbs.filter(
      (kb) =>
        kb.name.toLowerCase().includes(needle) ||
        (kb.description && kb.description.toLowerCase().includes(needle)),
    );
  }, [kbs, q]);

  return (
    <SectionFrame
      title={t('kb.listTitle')}
      count={filtered.length}
      newLabel={t('kb.newKb')}
      onNew={onNew}
      onSettings={onSettings}
      settingsLabel={t('knowledge.sharedEmbeddingTitle')}
    >
      <div className="relative">
        <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={qInput}
          onChange={(e) => setQInput(e.target.value)}
          placeholder={t('kb.searchPlaceholder')}
          aria-label={t('kb.searchPlaceholder')}
          className="h-9 pl-8"
        />
      </div>
      {filtered.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {q.trim() ? t('wiki.noSearchResults') : t('kb.empty')}
          </CardContent>
        </Card>
      ) : (
        filtered.map((kb) => (
          <Card
            key={kb.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === kb.id && 'border-primary/60 bg-primary/5',
            )}
            onClick={() => onSelect(kb.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <span className="truncate font-medium">{kb.name}</span>
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
    </SectionFrame>
  );
}
