import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Check, ChevronDown } from 'lucide-react';
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
import { listAgentSelectableModels } from '../../api/agentModels';
import ModelPicker from './ModelPicker';

interface Props {
  model: string;
  onModelChange: (id: string) => void;
  /** 已过滤掉 mode/thought_level 的其余 options（agent 内部 model、fast、persona 等） */
  configOptions: SessionConfigOption[];
  onConfigChange: (configId: string, value: string) => void;
  disabled?: boolean;
}

/** 统一会话设置菜单（输入框左下，原 ModelSelect 位置）：网关模型 +
 *  其他 config options（mode/effort 在右侧快捷按钮，不在此菜单）。 */
export default function SessionSettingsMenu({
  model,
  onModelChange,
  configOptions,
  onConfigChange,
  disabled,
}: Props) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });

  const modelLabel =
    data?.models.find((m) => m.id === model)?.label ??
    data?.groups.find((g) => g.id === model)?.label ??
    model ??
    t('agent.sessionSettings');

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
          className="h-7 w-auto gap-1 px-2 text-xs text-muted-foreground hover:bg-accent"
        >
          {modelLabel}
          <ChevronDown className="h-3 w-3" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-56">
        <ModelPicker
          models={data?.models ?? []}
          groups={data?.groups ?? []}
          currentModel={model}
          onSelect={onModelChange}
          disabled={disabled}
        />
        {configOptions.length > 0 && <DropdownMenuSeparator />}
        {configOptions.map((o) =>
          renderOptionSub(o, o.category === 'model' ? t('agent.agentModelHint') : undefined),
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
