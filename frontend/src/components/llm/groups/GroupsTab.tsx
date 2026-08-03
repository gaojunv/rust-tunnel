import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, Layers } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import {
  useLlmModelGroups,
  useUpdateLlmModelGroup,
  useDeleteLlmModelGroup,
} from '@/api/hooks';
import { GroupDialog } from './GroupDialog';

/** 模型组列表 tab：卡片式，显示组名/启用/成员数，点卡片打开编辑对话框。 */
export function GroupsTab() {
  const { t } = useTranslation();
  const { data: groups, isLoading } = useLlmModelGroups();
  const updateGroup = useUpdateLlmModelGroup();
  const deleteGroup = useDeleteLlmModelGroup();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <Button
          onClick={() => {
            setEditingId(null);
            setDialogOpen(true);
          }}
        >
          <Plus className="mr-2 h-4 w-4" />
          {t('llm.groups.add')}
        </Button>
      </div>

      {isLoading && <div className="text-muted-foreground">{t('common.loading')}</div>}

      {groups?.length === 0 && (
        <Card>
          <CardContent className="flex flex-col items-center gap-2 py-10 text-muted-foreground">
            <Layers className="h-8 w-8" />
            <div>{t('llm.groups.empty')}</div>
          </CardContent>
        </Card>
      )}

      {groups?.map((g) => (
        <Card
          key={g.id}
          className="cursor-pointer hover:border-primary/50"
          onClick={() => {
            setEditingId(g.id);
            setDialogOpen(true);
          }}
        >
          <CardHeader className="flex flex-row items-center justify-between space-y-0">
            <CardTitle className="flex items-center gap-2 text-base">
              {g.name}
              <Badge variant="secondary">
                {t('llm.groups.memberCount', { count: g.member_count })}
              </Badge>
              {!g.enabled && <Badge variant="outline">{t('common.disabled')}</Badge>}
            </CardTitle>
            <div onClick={(e) => e.stopPropagation()}>
              <Switch
                checked={g.enabled}
                onCheckedChange={(enabled) =>
                  updateGroup.mutate({ id: g.id, name: g.name, enabled })
                }
              />
            </div>
          </CardHeader>
        </Card>
      ))}

      <GroupDialog
        open={dialogOpen}
        onOpenChange={(open) => {
          setDialogOpen(open);
          if (!open) setEditingId(null);
        }}
        groupId={editingId}
        onDelete={(id) => {
          deleteGroup.mutate(id);
          setDialogOpen(false);
          setEditingId(null);
        }}
      />
    </div>
  );
}
