import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, ChevronUp } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { SessionConfigOption } from '../../types';
import { currentOptionLabel } from './sessionConfig';

interface Props {
  /** category=mode 的 ACP 选项；undefined（非 ACP 会话/模型不支持）时退化 */
  modeOption: SessionConfigOption | undefined;
  /** category=thought_level 的 ACP 选项 */
  effortOption: SessionConfigOption | undefined;
  onChange: (configId: string, value: string) => void;
  disabled?: boolean;
  /** agent 已上报过 config options 但 mode/effort 全缺（当前模型都不支持）：
   *  渲染禁用占位而非隐藏，hover 提示原因 */
  placeholder?: boolean;
}

/** 可渲染的取值列表：ACP 上报的 select 型且取值非空（boolean 项在左侧统一菜单里） */
const isSelectable = (o: SessionConfigOption | undefined): o is SessionConfigOption =>
  !!o && o.type === 'select' && !!o.options && o.options.length > 0;

/**
 * Mode + Effort 合并选择器（Claude Code 形态）：发送按钮左侧只显示当前 mode 的
 * 胶囊，点击向上弹出单一面板——上半为 mode 取值列表（选中即关闭），下半为
 * effort 单行（内联标题 + 当前档名 + 离散滑条，调档不关面板）。
 * 两项都走同一个 set_config_option 通道（onChange），乐观更新/回滚在 ChatStream。
 */
export default function ModeEffortPicker({
  modeOption,
  effortOption,
  onChange,
  disabled,
  placeholder,
}: Props) {
  const { t } = useTranslation();
  const mode = isSelectable(modeOption) ? modeOption : undefined;
  const effort = isSelectable(effortOption) ? effortOption : undefined;

  const effortValues = effort?.options ?? [];
  const lastIdx = effortValues.length - 1;
  // 当前值不在取值列表里（agent 换模型后档位表变了）时回退到首档，滑条不留空位
  const currentIdx = Math.max(
    0,
    effortValues.findIndex((v) => v.value === effort?.currentValue),
  );

  // 拖动中的即时视觉档位；null = 跟随服务端值
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  // 服务端权威值变化（乐观更新/回推/回滚）后丢弃本地拖动态，否则失败回滚会被
  // 停留的乐观视觉盖住
  useEffect(() => {
    setDragIdx(null);
  }, [effort?.currentValue]);

  const shownIdx = dragIdx ?? currentIdx;

  // 松手才发帧：拖过多档只产生一次 set_config_option，不把中间档位刷给 agent。
  // pending 放 ref，面板关闭时 flush——漏掉 release 事件也不丢用户的选择。
  const pendingRef = useRef<number | null>(null);
  const commitIdx = (idx: number) => {
    pendingRef.current = null;
    if (!effort) return;
    const target = effortValues[idx];
    // 与当前值相同（点回原档/仅聚焦）不发帧，保持幂等
    if (!target || target.value === effort.currentValue) return;
    onChange(effort.id, target.value);
  };
  const commitPending = () => {
    const idx = pendingRef.current;
    if (idx != null) commitIdx(idx);
  };

  if (!mode && !effort) {
    // 非 ACP 会话/未就绪：保持隐藏。已上报 options 但两项都缺：禁用占位 + 原因提示。
    if (!placeholder) return null;
    return (
      <Button
        variant="ghost"
        disabled
        aria-label={t('agent.configMode')}
        title={t('agent.configOptionUnsupported')}
        className="h-7 w-auto cursor-not-allowed rounded-full px-2.5 text-xs font-medium text-muted-foreground opacity-60"
      >
        {t('agent.configMode')}
      </Button>
    );
  }

  return (
    <DropdownMenu
      onOpenChange={(open) => {
        if (!open) commitPending();
      }}
    >
      <DropdownMenuTrigger asChild disabled={disabled}>
        <Button
          variant="ghost"
          // 外部只显示 mode；mode 不支持时退化成 effort，aria-label 随之切换
          aria-label={mode ? t('agent.configMode') : t('agent.configEffort')}
          className="h-7 w-auto rounded-full px-2.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          {currentOptionLabel(mode ?? effort!)}
          <ChevronUp className="ml-1 h-3 w-3 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" side="top" className="w-72">
        {mode && (
          <>
            <DropdownMenuLabel className="text-xs font-medium text-muted-foreground">
              {mode.name || t('agent.configMode')}
            </DropdownMenuLabel>
            {mode.options?.map((v) => (
              <DropdownMenuItem
                key={v.value}
                onSelect={() => onChange(mode.id, v.value)}
                disabled={disabled}
                className="flex items-start justify-between gap-3 text-xs"
              >
                <span className="flex min-w-0 flex-col gap-0.5">
                  <span className="truncate">{v.name}</span>
                  {v.description && (
                    <span className="text-[11px] leading-snug text-muted-foreground">
                      {v.description}
                    </span>
                  )}
                </span>
                {v.value === mode.currentValue && <Check className="mt-0.5 h-3 w-3 shrink-0" />}
              </DropdownMenuItem>
            ))}
            <DropdownMenuSeparator />
          </>
        )}
        {/* Effort 单行。借 DropdownMenuItem 只为让键盘焦点能落到这一行（Radix menu
            仅在 item 间用上下键导航），onSelect preventDefault 使 Enter/点击都不关面板；
            左右键在此调档（滑条自身聚焦时由原生 range 处理，见 input 的 stopPropagation）。 */}
        <DropdownMenuItem
          onSelect={(e) => e.preventDefault()}
          className="flex items-center gap-2 text-xs focus:bg-transparent"
          onKeyDown={(e) => {
            if (!effort || (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight')) return;
            e.preventDefault();
            e.stopPropagation();
            const next = Math.min(lastIdx, Math.max(0, shownIdx + (e.key === 'ArrowRight' ? 1 : -1)));
            setDragIdx(next);
            commitIdx(next); // 键盘无「松手」，每档即时提交
          }}
        >
          <span className="shrink-0 text-muted-foreground">{t('agent.configEffort')}</span>
          {effort ? (
            <>
              <span className="shrink-0 font-medium text-foreground">
                {effortValues[shownIdx]?.name ?? ''}
              </span>
              <span className="relative flex flex-1 items-center">
                {/* 档位刻度：提示可选档数，不参与命中 */}
                <span
                  aria-hidden
                  className="pointer-events-none absolute inset-x-[6px] top-1/2 flex -translate-y-1/2 justify-between"
                >
                  {effortValues.map((v) => (
                    <span key={v.value} className="h-1 w-1 rounded-full bg-border" />
                  ))}
                </span>
                <input
                  type="range"
                  min={0}
                  max={lastIdx}
                  step={1}
                  value={shownIdx}
                  disabled={disabled}
                  aria-label={t('agent.configEffort')}
                  className="effort-slider relative z-10 w-full"
                  onChange={(e) => {
                    const i = Number(e.target.value);
                    pendingRef.current = i;
                    setDragIdx(i);
                  }}
                  onPointerUp={commitPending}
                  onClick={commitPending}
                  onKeyUp={commitPending}
                  onTouchEnd={commitPending}
                  // 阻止冒泡：外层 item 的左右键处理与 Radix typeahead 都会与
                  // 原生 range 的键盘调档打架（双跳档 / 抢按键）
                  onKeyDown={(e) => e.stopPropagation()}
                />
              </span>
            </>
          ) : (
            <span className="text-[11px] leading-snug text-muted-foreground">
              {t('agent.configOptionUnsupported')}
            </span>
          )}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
