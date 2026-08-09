import { useTranslation } from 'react-i18next';
import { Check, ChevronDown } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { SessionConfigOption } from '../../types';
import { currentOptionLabel } from './sessionConfig';

interface Props {
  /** 单个 select 型 option；undefined（非 ACP 会话/agent 不上报）时不渲染 */
  option: SessionConfigOption | undefined;
  /** aria-label i18n 文案（agent.configMode / agent.configEffort） */
  label: string;
  onChange: (configId: string, value: string) => void;
  disabled?: boolean;
}

/** 发送按钮左侧的 config option 快捷按钮（VS Code Claude Code 插件样式）：
 *  幽灵小按钮显示当前取值名，点击向上弹出取值列表（当前项打勾）。 */
export default function ConfigOptionButton({ option, label, onChange, disabled }: Props) {
  const { t } = useTranslation();
  if (!option || option.type !== 'select') return null;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={disabled}>
        <Button
          variant="ghost"
          aria-label={t(label, { defaultValue: label })}
          className="h-7 w-auto gap-1 px-2 text-xs text-muted-foreground hover:bg-accent"
        >
          {currentOptionLabel(option)}
          <ChevronDown className="h-3 w-3" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" side="top">
        {option.options?.map((v) => (
          <DropdownMenuItem
            key={v.value}
            onSelect={() => onChange(option.id, v.value)}
            className="flex justify-between gap-4 text-xs"
          >
            <span>{v.name}</span>
            {v.value === option.currentValue && <Check className="h-3 w-3" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
