import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ArrowDown, ArrowUp, Trash2, RotateCcw } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  useLlmModelGroup,
  useCreateLlmModelGroup,
  useUpdateLlmModelGroup,
  useReplaceGroupMembers,
  useResetGroupBreaker,
  useLlmAllModels,
  useLlmProviders,
} from '@/api/hooks';

interface MemberRow {
  model_id: string;
  model_name: string;
  provider_name: string;
}

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  groupId: string | null; // null = 新建
  onDelete: (id: string) => void;
}

/** 组编辑对话框：组名 + 成员选择 + 上移/下移排序 + 熔断状态展示 + 重置熔断。 */
export function GroupDialog({ open, onOpenChange, groupId, onDelete }: Props) {
  const { t } = useTranslation();
  const { data: detail } = useLlmModelGroup(groupId ?? undefined);
  const { data: models } = useLlmAllModels();
  const { data: providers } = useLlmProviders();
  const createGroup = useCreateLlmModelGroup();
  const updateGroup = useUpdateLlmModelGroup();
  const replaceMembers = useReplaceGroupMembers();
  const resetBreaker = useResetGroupBreaker();

  const [name, setName] = useState('');
  const [members, setMembers] = useState<MemberRow[]>([]);
  const [pickModelId, setPickModelId] = useState('');
  // 新建成功后缓存服务端分配 id，save 部分失败重试时复用（走 update 分支），避免重复建组
  const [createdId, setCreatedId] = useState<string | null>(null);
  // 记录本次 (open, groupId) 是否已完成编辑态回填，防止 5s 熔断轮询的 detail 刷新覆盖用户编辑
  const backfilledRef = useRef(false);

  // 打开/切换组时重置编辑态（detail 仅用于熔断展示，不参与这里的依赖）
  useEffect(() => {
    setName('');
    setMembers([]);
    setPickModelId('');
    setCreatedId(null);
    backfilledRef.current = false;
  }, [open, groupId]);

  // 编辑模式回填一次：detail 到达后初始化 name/members
  useEffect(() => {
    if (!open || backfilledRef.current || !detail) return;
    setName(detail.name);
    setMembers(
      detail.members.map((m) => ({
        model_id: m.model_id,
        model_name: m.model_name,
        provider_name: m.provider_name,
      })),
    );
    backfilledRef.current = true;
  }, [open, groupId, detail]);

  const providerNameOf = (pid: string) =>
    providers?.find((p) => p.id === pid)?.name ?? '';

  const addMember = () => {
    const m = models?.find((mo) => mo.id === pickModelId);
    if (!m || members.some((x) => x.model_id === m.id)) return;
    setMembers([
      ...members,
      { model_id: m.id, model_name: m.alias || m.model_name, provider_name: providerNameOf(m.provider_id) },
    ]);
    setPickModelId('');
  };

  const move = (idx: number, dir: -1 | 1) => {
    const j = idx + dir;
    if (j < 0 || j >= members.length) return;
    const next = [...members];
    [next[idx], next[j]] = [next[j], next[idx]];
    setMembers(next);
  };

  const save = async () => {
    try {
      const effectiveId = groupId ?? createdId;
      let id = effectiveId;
      if (!id) {
        const created = await createGroup.mutateAsync({ name });
        id = created.id;
        setCreatedId(created.id);
      } else {
        await updateGroup.mutateAsync({ id, name });
      }
      await replaceMembers.mutateAsync({
        id,
        members: members.map((m, i) => ({ model_id: m.model_id, priority: i + 1 })),
      });
      onOpenChange(false);
    } catch {
      // 保存失败时保持对话框打开，让用户修正后重试
    }
  };

  // 熔断状态徽标
  const breakerBadge = (modelId: string) => {
    const m = detail?.members.find((x) => x.model_id === modelId);
    if (!m) return null;
    if (m.breaker.state === 'closed') {
      return <Badge variant="secondary">{t('llm.groups.breaker.closed')}</Badge>;
    }
    if (m.breaker.state === 'halfopenprobe') {
      return <Badge variant="outline">{t('llm.groups.breaker.halfOpen')}</Badge>;
    }
    return (
      <Badge variant="destructive">
        {t('llm.groups.breaker.open', { secs: m.breaker.cooldown_remaining_secs })}
      </Badge>
    );
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {groupId ? t('llm.groups.edit') : t('llm.groups.add')}
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4">
          <div>
            <label className="text-sm text-muted-foreground">{t('llm.groups.name')}</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="smart-router" />
          </div>

          <div className="space-y-2">
            <label className="text-sm text-muted-foreground">{t('llm.groups.members')}</label>
            {members.map((m, i) => (
              <div key={m.model_id} className="flex items-center gap-2 rounded border p-2">
                <span className="text-xs text-muted-foreground w-6">#{i + 1}</span>
                <span className="flex-1 text-sm">
                  {m.model_name}
                  <span className="ml-2 text-xs text-muted-foreground">{m.provider_name}</span>
                </span>
                {breakerBadge(m.model_id)}
                <Button size="icon" variant="ghost" onClick={() => move(i, -1)} disabled={i === 0}>
                  <ArrowUp className="h-4 w-4" />
                </Button>
                <Button size="icon" variant="ghost" onClick={() => move(i, 1)} disabled={i === members.length - 1}>
                  <ArrowDown className="h-4 w-4" />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  onClick={() => setMembers(members.filter((_, j) => j !== i))}
                >
                  <Trash2 className="h-4 w-4" />
                </Button>
              </div>
            ))}
            <div className="flex gap-2">
              <Select value={pickModelId} onValueChange={setPickModelId}>
                <SelectTrigger className="flex-1">
                  <SelectValue placeholder={t('llm.groups.pickModel')} />
                </SelectTrigger>
                <SelectContent>
                  {models
                    ?.filter((m) => !members.some((x) => x.model_id === m.id))
                    .map((m) => (
                      <SelectItem key={m.id} value={m.id}>
                        {m.alias || m.model_name} · {providerNameOf(m.provider_id)}
                      </SelectItem>
                    ))}
                </SelectContent>
              </Select>
              <Button variant="outline" onClick={addMember} disabled={!pickModelId}>
                {t('common.add')}
              </Button>
            </div>
          </div>

          <div className="flex items-center justify-between">
            <div className="flex gap-2">
              {groupId && (
                <>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => resetBreaker.mutate(groupId)}
                  >
                    <RotateCcw className="mr-1 h-4 w-4" />
                    {t('llm.groups.resetBreaker')}
                  </Button>
                  <Button variant="destructive" size="sm" onClick={() => onDelete(groupId)}>
                    {t('common.delete')}
                  </Button>
                </>
              )}
            </div>
            <Button onClick={save} disabled={!name}>
              {t('common.save')}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
