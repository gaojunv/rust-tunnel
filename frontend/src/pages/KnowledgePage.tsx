import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import { PageHeader } from '@/components/layout/PageHeader';
import KbSection from '@/components/knowledge/KbSection';
import MemorySection from '@/components/knowledge/MemorySection';
import SkillSection from '@/components/knowledge/SkillSection';
import RoleSection from '@/components/agent/role/RoleSection';
import WikiSection from '@/components/knowledge/WikiSection';
import { useLlmKbs, useMemories, useSkills, useRoles, useWikis } from '@/api/hooks';
import { BookOpen, Brain, Wrench, Users, FileText } from 'lucide-react';

const KNOW_TAB_VALUES = ['kb', 'memory', 'skill', 'roles', 'wiki'] as const;
type KnowTab = (typeof KNOW_TAB_VALUES)[number];
const DEFAULT_KNOW_TAB: KnowTab = 'kb';

const KNOW_TABS: { value: KnowTab; icon: React.ReactNode; labelKey: string }[] = [
  { value: 'kb', icon: <BookOpen className="h-4 w-4" />, labelKey: 'knowledge.section.kb' },
  { value: 'memory', icon: <Brain className="h-4 w-4" />, labelKey: 'knowledge.section.memory' },
  { value: 'skill', icon: <Wrench className="h-4 w-4" />, labelKey: 'knowledge.section.skill' },
  { value: 'roles', icon: <Users className="h-4 w-4" />, labelKey: 'knowledge.section.roles' },
  { value: 'wiki', icon: <FileText className="h-4 w-4" />, labelKey: 'knowledge.section.wiki' },
];

function useCounts() {
  const { data: kbs } = useLlmKbs();
  const { data: mem } = useMemories({});
  const { data: skills } = useSkills({});
  const { data: roles } = useRoles({});
  const { data: wikis } = useWikis({});
  return {
    kb: kbs?.length ?? 0,
    memory: mem?.total ?? mem?.memories?.length ?? 0,
    skill: skills?.total ?? skills?.skills?.length ?? 0,
    roles: roles?.total ?? roles?.roles?.length ?? 0,
    wiki: wikis?.total ?? wikis?.wikis?.length ?? 0,
  };
}

export default function KnowledgePage() {
  const { t: tTyped } = useTranslation();
  const t = tTyped as unknown as (k: string) => string;
  const [searchParams, setSearchParams] = useSearchParams();
  const raw = searchParams.get('tab');
  const activeTab: KnowTab = (KNOW_TAB_VALUES as readonly string[]).includes(raw ?? '')
    ? (raw as KnowTab)
    : DEFAULT_KNOW_TAB;
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
          {activeTab === 'kb' && <KbSection />}
          {activeTab === 'memory' && <MemorySection />}
          {activeTab === 'skill' && <SkillSection />}
          {activeTab === 'roles' && <RoleSection />}
          {activeTab === 'wiki' && <WikiSection />}
        </div>
      </div>
    </div>
  );
}
