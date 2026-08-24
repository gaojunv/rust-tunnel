import { Card, CardContent } from '@/components/ui/card';

interface Props {
  list: React.ReactNode;
  detail: React.ReactNode;
  hasSelection: boolean;
  emptyText: string;
  isLoading: boolean;
  loadingText: string;
}

/** 双栏 master-detail 外壳：左栏固定 320px，移动端选中后隐藏列表。 */
export default function MasterDetail({ list, detail, hasSelection, emptyText, isLoading, loadingText }: Props) {
  return (
    <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
      <div className={hasSelection ? 'hidden lg:block lg:w-80 lg:shrink-0' : 'lg:w-80 lg:shrink-0'}>
        {isLoading ? (
          <Card>
            <CardContent className="p-6 text-sm text-muted-foreground">{loadingText}</CardContent>
          </Card>
        ) : (
          list
        )}
      </div>
      <div className="min-w-0 flex-1">
        {hasSelection ? (
          detail
        ) : (
          <Card>
            <CardContent className="p-8 text-center text-sm text-muted-foreground">{emptyText}</CardContent>
          </Card>
        )}
      </div>
    </div>
  );
}
