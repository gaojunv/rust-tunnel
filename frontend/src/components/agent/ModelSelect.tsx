import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { listAgentSelectableModels } from '../../api/agentModels';

interface Props {
  value: string;
  onChange: (id: string) => void;
  disabled?: boolean;
}

/** 输入框内嵌的模型选择下拉：分「模型」「模型组」两组平铺，幽灵样式无边框。 */
export default function ModelSelect({ value, onChange, disabled }: Props) {
  const { t } = useTranslation();
  const { data } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });

  return (
    <Select value={value} onValueChange={onChange} disabled={disabled}>
      <SelectTrigger
        aria-label={t('agent.selectModel')}
        className="h-7 w-auto gap-1 border-0 bg-transparent px-2 text-xs text-muted-foreground shadow-none hover:bg-accent focus:ring-0"
      >
        <SelectValue placeholder={t('agent.selectModel')} />
      </SelectTrigger>
      <SelectContent>
        <SelectGroup>
          <SelectLabel>{t('agent.model')}</SelectLabel>
          {data?.models.map((m) => (
            <SelectItem key={m.id} value={m.id}>
              {m.label}
            </SelectItem>
          ))}
        </SelectGroup>
        {!!data?.groups.length && (
          <SelectGroup>
            <SelectLabel>{t('agent.modelGroups')}</SelectLabel>
            {data.groups.map((g) => (
              <SelectItem key={g.id} value={g.id}>
                {g.label}
              </SelectItem>
            ))}
          </SelectGroup>
        )}
      </SelectContent>
    </Select>
  );
}
