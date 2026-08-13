import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Label } from '@/components/ui/label';
import { ListChecks, Check, X, Ban } from 'lucide-react';
import type { ChatItem } from './types';
import type { ElicitationEnumOption, ElicitationPropertySchema } from '../../types';

interface Props {
  item: ChatItem;
  onRespond: (
    id: string,
    action: 'accept' | 'decline' | 'cancel',
    content?: Record<string, unknown>,
  ) => void;
}

/** claude-agent-acp 的 AskUserQuestion 「Other」自由文本字段标记（_meta 键） */
const CUSTOM_ANSWER_KEY = '_askUserQuestionCustomAnswer';

/** 选项 preview 展示（_meta._claude.askUserQuestionOption.preview，可选） */
const optionPreview = (opt: ElicitationEnumOption): string | undefined =>
  opt._meta?._claude?.askUserQuestionOption?.preview;

export default function ElicitationCard({ item, onRespond }: Props) {
  const { t } = useTranslation();
  const status = item.elicitationStatus ?? 'pending';
  const pending = status === 'pending';
  const schema = item.elicitationSchema;
  const properties = schema?.properties ?? {};

  // 表单值 state：初值取各 property 的 default
  const [values, setValues] = useState<Record<string, unknown>>(() => {
    const init: Record<string, unknown> = {};
    for (const [key, prop] of Object.entries(properties)) {
      if (prop.default !== undefined) init[key] = prop.default;
    }
    return init;
  });

  const setValue = (key: string, v: unknown) =>
    setValues((prev) => ({ ...prev, [key]: v }));

  const isRequired = (key: string) => schema?.required?.includes(key) ?? false;

  // 必填门控：required 中字段为空（undefined/null/空串/空数组）视为未填 → 提交禁用
  const requiredFilled = (schema?.required ?? []).every((key) => {
    const v = values[key];
    if (v === undefined || v === null) return false;
    if (typeof v === 'string') return v.trim() !== '';
    if (Array.isArray(v)) return v.length > 0;
    return true;
  });

  const handleSubmit = () => {
    if (!pending || !requiredFilled) return;
    // 只带已填写的字段（空值不落 content；ChatStream 侧空 content 不发 content 键）
    const content = Object.fromEntries(
      Object.entries(values).filter(
        ([, v]) =>
          v !== undefined &&
          v !== null &&
          !(typeof v === 'string' && v.trim() === '') &&
          !(Array.isArray(v) && v.length === 0),
      ),
    );
    if (item.elicitationId) onRespond(item.elicitationId, 'accept', content);
  };
  const handleDecline = () => item.elicitationId && onRespond(item.elicitationId, 'decline');
  const handleCancel = () => item.elicitationId && onRespond(item.elicitationId, 'cancel');

  // 多选 toggle：选中进数组 state，再点移除
  const toggleMulti = (key: string, constVal: string) => {
    setValues((prev) => {
      const cur = Array.isArray(prev[key]) ? (prev[key] as string[]) : [];
      const next = cur.includes(constVal)
        ? cur.filter((v) => v !== constVal)
        : [...cur, constVal];
      return { ...prev, [key]: next };
    });
  };

  // 三态视觉区分：pending 琥珀高亮；accepted emerald；declined destructive；
  // cancelled 被动取消 → 灰色 + 虚线边框（区别于用户主动跳过）
  const CARD_CLS: Record<string, string> = {
    pending: 'border-amber-500/50 bg-amber-500/10',
    accepted: 'border-emerald-500/50 bg-emerald-500/10',
    declined: 'border-destructive/50 bg-destructive/10',
    cancelled: 'border-dashed border-border bg-muted/30 opacity-70',
  };
  const ICON_CLS: Record<string, string> = {
    pending: 'text-amber-500',
    accepted: 'text-emerald-600',
    declined: 'text-destructive',
    cancelled: 'text-muted-foreground',
  };
  // 终态徽章（替换底部按钮）：accepted/declined 用户主动处理，cancelled 被动取消
  const BADGE_CLS: Record<string, string> = {
    accepted: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600',
    declined: 'border-destructive/40 bg-destructive/10 text-destructive',
    cancelled: 'border-border bg-muted text-muted-foreground',
  };
  const badgeText =
    status === 'accepted' ? `✓ ${t('agent.elicitationAnswered')}`
    : status === 'declined' ? `✗ ${t('agent.elicitationDeclined')}`
    : t('agent.elicitationCancelled');

  /** 按 property schema 逐字段渲染控件（单选按钮组 / Other 输入 / 多选 toggle / Switch / number / text） */
  const renderField = (key: string, prop: ElicitationPropertySchema) => {
    const label = prop.title ?? key;
    const controlId = `elic-${key}`;
    // 单选：string + oneOf/enum → 按钮组（选中项高亮 primary）
    const oneOf: ElicitationEnumOption[] | undefined =
      prop.oneOf && prop.oneOf.length > 0
        ? prop.oneOf
        : prop.type === 'string' && prop.enum && prop.enum.length > 0
          ? prop.enum.map((v) => ({ const: v, title: v }))
          : undefined;
    // 多选：array + items.anyOf/enum → toggle 按钮组（aria-pressed 语义，无 checkbox 封装）
    const multiOptions: ElicitationEnumOption[] | undefined =
      prop.type === 'array'
        ? prop.items?.anyOf && prop.items.anyOf.length > 0
          ? prop.items.anyOf
          : prop.items?.enum && prop.items.enum.length > 0
            ? prop.items.enum.map((v) => ({ const: v, title: v }))
            : undefined
        : undefined;
    const isCustomAnswer = prop._meta ? CUSTOM_ANSWER_KEY in prop._meta : false;

    return (
      <div key={key} className="mb-2">
        <Label htmlFor={controlId} className="mb-1 block text-xs font-medium">
          {label}
          {isRequired(key) && <span className="ml-0.5 text-destructive">*</span>}
        </Label>
        {prop.description && (
          <p className="mb-1 text-[11px] text-muted-foreground">{prop.description}</p>
        )}
        {oneOf ? (
          <div className="flex flex-col gap-1.5">
            {oneOf.map((opt) => {
              const selected = values[key] === opt.const;
              const preview = optionPreview(opt);
              return (
                <Button
                  key={opt.const}
                  type="button"
                  size="sm"
                  variant={selected ? 'default' : 'outline'}
                  className={`justify-start ${selected ? '' : 'border-border text-foreground hover:bg-muted'}`}
                  aria-pressed={selected}
                  disabled={!pending}
                  onClick={() => setValue(key, opt.const)}
                >
                  {selected && <Check className="mr-1 h-3.5 w-3.5 shrink-0" />}
                  <span className="flex min-w-0 flex-col items-start">
                    <span>{opt.title}</span>
                    {preview && (
                      <span className="text-[11px] text-muted-foreground">{preview}</span>
                    )}
                  </span>
                </Button>
              );
            })}
          </div>
        ) : isCustomAnswer ? (
          // AskUserQuestion 的「Other」自由文本：占位提示复用 elicitationOther
          <Input
            id={controlId}
            type="text"
            placeholder={t('agent.elicitationOther')}
            value={(values[key] ?? '') as string}
            disabled={!pending}
            onChange={(e) => setValue(key, e.target.value)}
          />
        ) : multiOptions ? (
          <div className="flex flex-wrap gap-1.5">
            {multiOptions.map((opt) => {
              const cur = Array.isArray(values[key]) ? (values[key] as string[]) : [];
              const selected = cur.includes(opt.const);
              return (
                <Button
                  key={opt.const}
                  type="button"
                  size="sm"
                  variant={selected ? 'default' : 'outline'}
                  className={selected ? '' : 'border-border text-foreground hover:bg-muted'}
                  aria-pressed={selected}
                  disabled={!pending}
                  onClick={() => toggleMulti(key, opt.const)}
                >
                  {selected && <Check className="mr-1 h-3.5 w-3.5" />}
                  {opt.title}
                </Button>
              );
            })}
          </div>
        ) : prop.type === 'boolean' ? (
          <div className="flex items-center gap-2">
            <Switch
              id={controlId}
              aria-label={label}
              checked={values[key] === true}
              disabled={!pending}
              onCheckedChange={(v) => setValue(key, v)}
            />
            <span className="text-xs text-muted-foreground">
              {values[key] === true ? t('agent.configOn') : t('agent.configOff')}
            </span>
          </div>
        ) : prop.type === 'number' || prop.type === 'integer' ? (
          <Input
            id={controlId}
            type="number"
            value={(values[key] ?? '') as number}
            disabled={!pending}
            onChange={(e) =>
              setValue(key, e.target.value === '' ? undefined : Number(e.target.value))
            }
          />
        ) : (
          // 其余 string / 未知 type：文本输入兜底
          <Input
            id={controlId}
            type="text"
            value={(values[key] ?? '') as string}
            disabled={!pending}
            onChange={(e) => setValue(key, e.target.value)}
          />
        )}
      </div>
    );
  };

  return (
    <div className={`rounded-lg border p-3 text-sm ${CARD_CLS[status]}`}>
      <div className="mb-1.5 flex items-center gap-1.5 font-medium">
        <ListChecks className={`h-4 w-4 ${ICON_CLS[status]}`} />
        <span className="min-w-0 flex-1 truncate">{t('agent.elicitationRequired')}</span>
        {!pending && (
          // shrink-0 + whitespace-nowrap：长文案只截断标题，不挤压终态徽章
          <span className={`ml-auto inline-flex shrink-0 items-center whitespace-nowrap rounded-full border px-2 py-0.5 text-[11px] font-medium ${BADGE_CLS[status]}`}>
            {badgeText}
          </span>
        )}
      </div>
      {item.elicitationMessage && (
        <div className="mb-2 whitespace-pre-wrap text-xs text-foreground/80">
          {item.elicitationMessage}
        </div>
      )}
      {pending && (
        <>
          {Object.entries(properties).map(([key, prop]) => renderField(key, prop))}
          <div className="mt-2 flex flex-wrap gap-2">
            <Button size="sm" variant="default" disabled={!requiredFilled} onClick={handleSubmit}>
              <Check className="mr-1 h-3.5 w-3.5" />{t('agent.elicitationSubmit')}
            </Button>
            <Button size="sm" variant="outline" onClick={handleDecline}>
              <X className="mr-1 h-3.5 w-3.5" />{t('agent.elicitationDecline')}
            </Button>
            <Button size="sm" variant="ghost" onClick={handleCancel}>
              <Ban className="mr-1 h-3.5 w-3.5" />{t('agent.elicitationCancel')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
