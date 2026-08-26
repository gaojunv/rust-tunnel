import { useCallback, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import { PageHeader } from '@/components/layout/PageHeader';
import SourceSection from '@/components/knowledge/source/SourceSection';
import MemorySection from '@/components/knowledge/MemorySection';
import SkillSection from '@/components/knowledge/SkillSection';
import RoleSection from '@/components/agent/role/RoleSection';
import { useKnowledgeSources, useMemories, useSkills, useRoles } from '@/api/hooks';
import { BookOpen, Brain, Wrench, Users } from 'lucide-react';

const KNOW_TAB_VALUES = ['kb', 'memory', 'skill', 'roles'] as const;
export type KnowTab = (typeof KNOW_TAB_VALUES)[number];
const DEFAULT_KNOW_TAB: KnowTab = 'kb';

/** 旧深链归一：Wiki 容器已与向量库合并为同一个知识容器，`?tab=wiki` 落到 `kb`。 */
const LEGACY_TAB_ALIAS: Record<string, KnowTab> = { wiki: 'kb' };

/** `?tab=` 归一。`aliased` 非空表示命中旧深链，调用方据此改写地址栏。 */
export function resolveKnowTab(raw: string | null): { tab: KnowTab; aliased: KnowTab | undefined } {
  const key = raw ?? '';
  const aliased = LEGACY_TAB_ALIAS[key];
  const tab = aliased ?? ((KNOW_TAB_VALUES as readonly string[]).includes(key) ? (key as KnowTab) : DEFAULT_KNOW_TAB);
  return { tab, aliased };
}

const KNOW_TABS: { value: KnowTab; icon: React.ReactNode; labelKey: string }[] = [
  { value: 'kb', icon: <BookOpen className="h-4 w-4" />, labelKey: 'knowledge.section.kb' },
  { value: 'memory', icon: <Brain className="h-4 w-4" />, labelKey: 'knowledge.section.memory' },
  { value: 'skill', icon: <Wrench className="h-4 w-4" />, labelKey: 'knowledge.section.skill' },
  { value: 'roles', icon: <Users className="h-4 w-4" />, labelKey: 'knowledge.section.roles' },
];

function useCounts() {
  const { data: sources } = useKnowledgeSources({});
  const { data: mem } = useMemories({});
  const { data: skills } = useSkills({});
  const { data: roles } = useRoles({});
  return {
    kb: sources?.total ?? sources?.sources?.length ?? 0,
    memory: mem?.total ?? mem?.memories?.length ?? 0,
    skill: skills?.total ?? skills?.skills?.length ?? 0,
    roles: roles?.total ?? roles?.roles?.length ?? 0,
  };
}

export default function KnowledgePage() {
  const { t: tTyped } = useTranslation();
  const t = tTyped as unknown as (k: string) => string;
  const [searchParams, setSearchParams] = useSearchParams();
  const { tab: activeTab, aliased } = resolveKnowTab(searchParams.get('tab'));
  const setActiveTab = useCallback(
    (v: string) => {
      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          next.set('tab', v);
          return next;
        },
        { replace: true },
      );
    },
    [setSearchParams],
  );
  // 命中旧别名时就地改写地址栏，避免 URL 与实际分区长期不一致
  useEffect(() => {
    if (aliased) setActiveTab(aliased);
  }, [aliased, setActiveTab]);
  const counts = useCounts();

  return (
    <div className="space-y-6">
      <PageHeader title={t('knowledge.title')} description={t('knowledge.description')} />

      {/* 移动端：横向 pill 条（可横滑，不换行） */}
      <div className="flex gap-1.5 overflow-x-auto pb-1 lg:hidden">
        {KNOW_TABS.map((tab) => {
          const active = tab.value === activeTab;
          return (
            <button
              key={tab.value}
              onClick={() => setActiveTab(tab.value)}
              className={cn(
                'inline-flex shrink-0 items-center gap-1.5 rounded-full border px-3 py-1.5 text-sm font-medium transition-colors',
                active
                  ? 'border-primary bg-primary text-primary-foreground'
                  : 'border-border bg-card text-muted-foreground hover:bg-accent hover:text-accent-foreground',
              )}
            >
              {tab.icon}
              {t(tab.labelKey)}
              <Badge
                variant={active ? 'secondary' : 'outline'}
                className={cn('ml-1 h-5 min-w-5 justify-center px-1 text-[10px]', active && 'bg-primary-foreground/15 text-primary-foreground')}
              >
                {counts[tab.value]}
              </Badge>
            </button>
          );
        })}
      </div>

      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        {/* 桌面：左侧竖向导航（sticky，随 ScrollArea 整页滚动而吸顶） */}
        <nav className="hidden w-52 shrink-0 lg:block">
          <div className="sticky top-6 space-y-1 rounded-xl border bg-card p-2">
            {KNOW_TABS.map((tab) => {
              const active = tab.value === activeTab;
              return (
                <button
                  key={tab.value}
                  onClick={() => setActiveTab(tab.value)}
                  className={cn(
                    'flex w-full items-center gap-2 rounded-lg px-3 py-2 text-sm font-medium transition-colors',
                    active
                      ? 'bg-primary text-primary-foreground'
                      : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground',
                  )}
                >
                  {tab.icon}
                  <span className="flex-1 text-left">{t(tab.labelKey)}</span>
                  <Badge
                    variant={active ? 'secondary' : 'outline'}
                    className={cn('h-5 min-w-5 justify-center px-1 text-[10px]', active && 'bg-primary-foreground/15 text-primary-foreground')}
                  >
                    {counts[tab.value]}
                  </Badge>
                </button>
              );
            })}
            <p className="px-3 pt-2 text-[11px] leading-relaxed text-muted-foreground">{t('knowledge.settingsHint')}</p>
          </div>
        </nav>

        <div className="min-w-0 flex-1">
          {activeTab === 'kb' && <SourceSection />}
          {activeTab === 'memory' && <MemorySection />}
          {activeTab === 'skill' && <SkillSection />}
          {activeTab === 'roles' && <RoleSection />}
        </div>
      </div>
    </div>
  );
}
