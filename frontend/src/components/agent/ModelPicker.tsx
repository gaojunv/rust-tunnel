import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, Search } from 'lucide-react';
import { Input } from '@/components/ui/input';
import {
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu';
import type { SelectableModel } from '../../api/agentModels';

interface Props {
  models: SelectableModel[];
  groups?: SelectableModel[];
  currentModel: string;
  onSelect: (id: string) => void;
  disabled?: boolean;
}

/** 匹配模型/组的 id 或 label（不区分大小写）。 */
const matchesQuery = (m: SelectableModel, q: string) =>
  m.id.toLowerCase().includes(q) || m.label.toLowerCase().includes(q);

/**
 * 模型扁平选择区：搜索输入 + 单层模型/模型组列表（替代原先嵌套子菜单）。
 * 纯展示子组件，不含 trigger——由外层 DropdownMenu 提供上下文；
 * 内部仅管理搜索过滤状态。用于 SessionSettingsMenu 菜单顶部，
 * 也可复用在独立触发按钮的 DropdownMenuContent 中。
 */
export default function ModelPicker({
  models,
  groups = [],
  currentModel,
  onSelect,
  disabled,
}: Props) {
  const { t } = useTranslation();
  const [query, setQuery] = useState('');

  const q = query.trim().toLowerCase();
  const filteredModels = q ? models.filter((m) => matchesQuery(m, q)) : models;
  const filteredGroups = q ? groups.filter((g) => matchesQuery(g, q)) : groups;
  const hasResults = filteredModels.length > 0 || filteredGroups.length > 0;

  return (
    <>
      <div className="sticky top-0 z-10 -mx-1 bg-popover px-1 pb-1">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            // 阻止按键冒泡到 DropdownMenu content，避免 Radix typeahead 抢占输入
            onKeyDown={(e) => e.stopPropagation()}
            placeholder={t('agent.searchModels')}
            className="h-8 pl-7 text-xs"
          />
        </div>
      </div>
      {filteredModels.length > 0 && (
        <>
          <DropdownMenuLabel className="text-xs">{t('agent.model')}</DropdownMenuLabel>
          {filteredModels.map((m) => (
            <DropdownMenuItem
              key={m.id}
              onSelect={() => onSelect(m.id)}
              disabled={disabled}
              className="flex justify-between gap-4 text-xs"
            >
              <span className="truncate">{m.label}</span>
              {m.id === currentModel && <Check className="h-3 w-3 shrink-0" />}
            </DropdownMenuItem>
          ))}
        </>
      )}
      {filteredGroups.length > 0 && (
        <>
          {filteredModels.length > 0 && <DropdownMenuSeparator />}
          <DropdownMenuLabel className="text-xs">{t('agent.modelGroups')}</DropdownMenuLabel>
          {filteredGroups.map((g) => (
            <DropdownMenuItem
              key={g.id}
              onSelect={() => onSelect(g.id)}
              disabled={disabled}
              className="flex justify-between gap-4 text-xs"
            >
              <span className="truncate">{g.label}</span>
              {g.id === currentModel && <Check className="h-3 w-3 shrink-0" />}
            </DropdownMenuItem>
          ))}
        </>
      )}
      {!hasResults && (
        <div className="px-2 py-1.5 text-xs text-muted-foreground">
          {t('agent.noModelsFound')}
        </div>
      )}
    </>
  );
}
