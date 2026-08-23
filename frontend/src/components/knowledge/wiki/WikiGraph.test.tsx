// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { WikiGraphEdge, WikiPageSummary } from '@/types';
import WikiGraph from './WikiGraph';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const nodes: WikiPageSummary[] = [
  {
    id: 'p1',
    wiki_id: 'w1',
    ref: 'deploy/prod',
    title: 'Production deploy',
    summary: '',
    locked: true,
    use_count: 8,
    source_doc_id: 'd1',
    last_used_at: null,
    created_at: '',
    updated_at: '',
  },
  {
    id: 'p2',
    wiki_id: 'w1',
    ref: 'deploy/checklist',
    title: '',
    summary: '',
    locked: false,
    use_count: 4,
    source_doc_id: null,
    last_used_at: null,
    created_at: '',
    updated_at: '',
  },
  {
    id: 'p3',
    wiki_id: 'w1',
    ref: 'ops/monitor',
    title: '',
    summary: '',
    locked: false,
    use_count: 1,
    source_doc_id: null,
    last_used_at: null,
    created_at: '',
    updated_at: '',
  },
];

const edges: WikiGraphEdge[] = [
  { from: 'p1', from_ref: 'deploy/prod', to: 'p2', to_ref: 'deploy/checklist', dangling: false },
  { from: 'p2', from_ref: 'deploy/checklist', to: 'p3', to_ref: 'ops/monitor', dangling: false },
  // 悬空边：目标页不存在
  { from: 'p1', from_ref: 'deploy/prod', to: null, to_ref: 'missing/page', dangling: true },
];

const renderGraph = (onNodeClick: (ref: string) => void = () => {}) =>
  render(<WikiGraph nodes={nodes} edges={edges} onNodeClick={onNodeClick} />);

describe('WikiGraph（jsdom 退化模式：静态网格布局）', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders all nodes with ref labels', () => {
    renderGraph();
    // ref 同时出现在节点 <text> 标签与 <title> 工具提示中，用 getAllByText
    expect(screen.getAllByText('deploy/prod').length).toBeGreaterThan(0);
    expect(screen.getAllByText('deploy/checklist').length).toBeGreaterThan(0);
    expect(screen.getAllByText('ops/monitor').length).toBeGreaterThan(0);
    // 每个节点一个 circle
    expect(document.querySelectorAll('g.wiki-node circle')).toHaveLength(3);
  });

  it('marks locked nodes with the locked class', () => {
    renderGraph();
    const lockedNode = document.querySelector('g.wiki-node-locked');
    expect(lockedNode).toBeTruthy();
    expect(lockedNode!.getAttribute('data-ref')).toBe('deploy/prod');
  });

  it('renders normal edges and dangling edges with dashed destructive class', () => {
    renderGraph();
    const normal = document.querySelectorAll('.wiki-edge:not(.wiki-edge-dangling)');
    expect(normal).toHaveLength(2);
    const dangling = document.querySelector('.wiki-edge-dangling');
    expect(dangling).toBeTruthy();
    expect(dangling!.getAttribute('stroke-dasharray')).toBe('6 4');
    // 悬空边的目标 ref 作为标签渲染
    expect(screen.getByText('missing/page')).toBeTruthy();
  });

  it('invokes onNodeClick with the ref when a node is clicked', () => {
    const onNodeClick = vi.fn();
    renderGraph(onNodeClick);
    const node = document.querySelector('g.wiki-node[data-ref="deploy/checklist"]')!;
    fireEvent.click(node.querySelector('circle')!);
    expect(onNodeClick).toHaveBeenCalledWith('deploy/checklist');
  });

  it('renders an empty state when there are no nodes', () => {
    render(<WikiGraph nodes={[]} edges={[]} />);
    expect(screen.getByText('wiki.graphEmpty')).toBeTruthy();
  });
});
