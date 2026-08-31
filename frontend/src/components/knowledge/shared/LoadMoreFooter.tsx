import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';

interface Props {
  loaded: number;
  total: number;
  loading?: boolean;
  onLoadMore: () => void;
}

/** 已加载计数 + 加载更多按钮；已全部加载时仅显示计数。 */
export default function LoadMoreFooter({ loaded, total, loading, onLoadMore }: Props) {
  const { t } = useTranslation() as unknown as { t: (k: string, opts?: Record<string, unknown>) => string };
  const hasMore = loaded < total;

  return (
    <div className="flex flex-col items-center gap-2 py-3">
      <span className="text-sm text-muted-foreground">{t('common.loadedOf', { loaded, total })}</span>
      {hasMore && (
        <Button variant="outline" size="sm" onClick={onLoadMore} disabled={loading}>
          {loading && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
          {t('common.loadMore')}
        </Button>
      )}
    </div>
  );
}
