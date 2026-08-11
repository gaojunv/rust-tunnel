import { useTranslation } from 'react-i18next';
import { Check } from 'lucide-react';
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
  /** agent 已上报过 config options 但本项缺失（如当前模型不支持 Effort）：
   *  渲染禁用占位而非隐藏，hover 提示原因 */
  placeholder?: boolean;
}

/** 发送按钮左侧的 config option 快捷按钮（VS Code Claude Code 插件样式）：
 *  幽灵小按钮显示当前取值名，点击向上弹出取值列表（当前项打勾）。 */
export default function ConfigOptionButton({ option, label, onChange, disabled, placeholder }: Props) {
  const { t } = useTranslation();
  if (!option || option.type !== 'select') {
    // 占位：agent 已上报过 config options 但缺本项（模型不支持）→ 禁用按钮。
    // 非 ACP 会话/未就绪时 placeholder 缺省 false，保持隐藏既有语义。
    if (placeholder) {
      return (
        <Button
          variant="ghost"
          disabled
          aria-label={t(label, { defaultValue: label })}
          title={t('agent.configOptionUnsupported')}
          className="h-7 w-auto cursor-not-allowed rounded-full px-2.5 text-xs font-medium text-muted-foreground opacity-60"
        >
          {t(label, { defaultValue: label })}
        </Button>
      );
    }
    return null;
  }
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={disabled}>
        <Button
          variant="ghost"
          aria-label={t(label, { defaultValue: label })}
          className="h-7 w-auto rounded-full px-2.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          {currentOptionLabel(option)}
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
