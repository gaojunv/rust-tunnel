import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Check } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { SessionConfigOption } from '../../types';
import { currentOptionLabel } from './sessionConfig';
import { listAgentSelectableModels, resolveWorkspaceModelRef } from '../../api/agentModels';
import { useRoles, useUpdateSessionRole } from '../../api/hooks';
import ModelPicker from './ModelPicker';

interface Props {
  model: string;
  onModelChange: (id: string) => void;
  /** 已过滤掉 mode/thought_level 的其余 options（agent 内部 model、fast、persona 等） */
  configOptions: SessionConfigOption[];
  onConfigChange: (configId: string, value: string) => void;
  disabled?: boolean;
  /** 当前会话角色 id（null=默认） */
  roleId?: string | null;
  /** 当前会话 id（切换角色时调用 PATCH） */
  sessionId?: string;
  claudeTierModels?: string | null;
  agentType?: string | null;
}

/** 统一会话设置菜单（输入框左下，原 ModelSelect 位置）：网关模型 + 会话角色 +
 *  其他 config options（mode/effort 在右侧快捷按钮，不在此菜单）。 */
export default function SessionSettingsMenu({
  model,
  onModelChange,
  configOptions,
  onConfigChange,
  disabled,
  roleId,
  sessionId,
  claudeTierModels,
  agentType,
}: Props) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });
  const { data: rolesData } = useRoles({ enabled: true });
  const updateRoleMutation = useUpdateSessionRole();

  // 只显示 mode 含 primary/all 的角色（可被选作主会话角色）
  const primaryRoles = (rolesData?.roles ?? []).filter(
    (r) => (r.mode === 'primary' || r.mode === 'all') && r.enabled,
  );

  const handleRoleChange = (newRoleId: string) => {
    if (!sessionId) return;
    const resolved = newRoleId === '' ? null : newRoleId;
    updateRoleMutation.mutate({ sessionId, roleId: resolved });
  };

  const currentRoleName = primaryRoles.find((r) => r.id === roleId)?.name ?? null;

  // 实际生效模型推导：优先展示后端实际裁决的模型（tier 映射 / 直通），回退到 session.model 链
  const { display: modelLabel, tierActive } = useMemo(() => {
    const fallback =
      data?.models.find((m) => m.id === model)?.label ??
      data?.groups.find((g) => g.id === model)?.label ??
      model ??
      t('agent.sessionSettings');
    const modelOption = configOptions.find((o) => o.category === 'model');
    const current =
      typeof modelOption?.currentValue === 'string' && modelOption.currentValue.trim()
        ? modelOption.currentValue.trim()
        : undefined;
    if (!current) return { display: fallback, tierActive: false };

    // 规则 1（tier 映射）：claude-code 且 current 为 tier 名时，按 workspace 的 tier 映射解析
    if (agentType === 'claude-code') {
      let tierMap: Record<string, string> | null = null;
      if (claudeTierModels) {
        try {
          const parsed = JSON.parse(claudeTierModels);
          if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
            tierMap = parsed as Record<string, string>;
          }
        } catch {
          // 解析失败视为无映射
        }
      }
      if (tierMap) {
        // claude-code 的 default 档即 sonnet tier，ANTHROPIC_DEFAULT_SONNET_MODEL 生效
        const tier = current === 'default' ? 'sonnet' : current;
        if (tier === 'opus' || tier === 'sonnet' || tier === 'haiku') {
          const rawRef = tierMap[tier];
          if (typeof rawRef === 'string' && rawRef.trim()) {
            const resolved = resolveWorkspaceModelRef(rawRef.trim(), data);
            if (resolved) {
              const label =
                data?.models.find((m) => m.id === resolved)?.label ??
                data?.groups.find((g) => g.id === resolved)?.label ??
                resolved;
              const tierCapitalized = tier.charAt(0).toUpperCase() + tier.slice(1);
              return {
                display: t('agent.tierModelLabel', { tier: tierCapitalized, model: label }),
                tierActive: true,
              };
            }
          }
        }
      }
    }

    // 规则 2（直通）：current 本身即为可选模型/组的 id
    const directLabel =
      data?.models.find((m) => m.id === current)?.label ??
      data?.groups.find((g) => g.id === current)?.label;
    if (directLabel) {
      return { display: directLabel, tierActive: false };
    }

    return { display: fallback, tierActive: false };
  }, [data, model, configOptions, claudeTierModels, agentType, t]);

  const renderOptionSub = (o: SessionConfigOption, hint?: string) => (
    <DropdownMenuSub key={o.id}>
      <DropdownMenuSubTrigger className="flex justify-between gap-4 text-xs">
        <span>{o.name}</span>
        <span className="text-muted-foreground">
          {o.type === 'boolean'
            ? o.currentBool
              ? t('agent.configOn')
              : t('agent.configOff')
            : currentOptionLabel(o)}
        </span>
      </DropdownMenuSubTrigger>
      <DropdownMenuSubContent>
        {hint && <DropdownMenuLabel className="text-xs text-muted-foreground">{hint}</DropdownMenuLabel>}
        {o.type === 'boolean' ? (
          <DropdownMenuCheckboxItem
            checked={o.currentBool}
            onCheckedChange={(checked) => onConfigChange(o.id, checked ? 'true' : 'false')}
          >
            {o.name}
          </DropdownMenuCheckboxItem>
        ) : (
          o.options?.map((v) => (
            <DropdownMenuItem
              key={v.value}
              onSelect={() => onConfigChange(o.id, v.value)}
              className="flex justify-between gap-4 text-xs"
            >
              <span>{v.name}</span>
              {v.value === o.currentValue && <Check className="h-3 w-3" />}
            </DropdownMenuItem>
          ))
        )}
      </DropdownMenuSubContent>
    </DropdownMenuSub>
  );

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={disabled}>
        <Button
          variant="ghost"
          aria-label={t('agent.sessionSettings')}
          className="h-7 w-auto rounded-full px-2.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          {modelLabel}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="top" className="w-56">
        {tierActive && (
          <DropdownMenuLabel className="text-xs text-muted-foreground">
            {t('agent.tierModelActiveHint')}
          </DropdownMenuLabel>
        )}
        <ModelPicker
          models={data?.models ?? []}
          groups={data?.groups ?? []}
          currentModel={model}
          onSelect={onModelChange}
          disabled={disabled}
        />
        {primaryRoles.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuSub>
              <DropdownMenuSubTrigger className="flex justify-between gap-4 text-xs">
                <span>{t('role.sessionRole')}</span>
                <span className="text-muted-foreground">
                  {currentRoleName ?? t('role.default')}
                </span>
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                <DropdownMenuItem
                  onSelect={() => handleRoleChange('')}
                  className="flex justify-between gap-4 text-xs"
                >
                  <span>{t('role.default')}</span>
                  {!roleId && <Check className="h-3 w-3" />}
                </DropdownMenuItem>
                {primaryRoles.map((r) => (
                  <DropdownMenuItem
                    key={r.id}
                    onSelect={() => handleRoleChange(r.id)}
                    className="flex justify-between gap-4 text-xs"
                  >
                    <span>{r.name}</span>
                    {roleId === r.id && <Check className="h-3 w-3" />}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          </>
        )}
        {configOptions.length > 0 && <DropdownMenuSeparator />}
        {configOptions.map((o) =>
          renderOptionSub(o, o.category === 'model' ? t('agent.agentModelHint') : undefined),
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
