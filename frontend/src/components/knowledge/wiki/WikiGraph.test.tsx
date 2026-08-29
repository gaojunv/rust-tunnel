// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { WikiGraphEdge, WikiPageSummary } from '@/types';
import WikiGraph, { shouldTreatAsClick } from './WikiGraph';

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
    expect(screen.getAllByText('deploy/prod').length).toBeGreaterThan(0);
    expect(screen.getAllByText('deploy/checklist').length).toBeGreaterThan(0);
    expect(screen.getAllByText('ops/monitor').length).toBeGreaterThan(0);
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
    expect(screen.getByText('missing/page')).toBeTruthy();
  });

  it('invokes onNodeClick with the ref when a node is clicked', () => {
    const onNodeClick = vi.fn();
    renderGraph(onNodeClick);
    const node = document.querySelector('g.wiki-node[data-ref="deploy/checklist"]')!;
    fireEvent.click(node);
    expect(onNodeClick).toHaveBeenCalledWith('deploy/checklist');
  });

  it('renders an empty state when there are no nodes', () => {
    render(<WikiGraph nodes={[]} edges={[]} />);
    expect(screen.getByText('wiki.graphEmpty')).toBeTruthy();
  });

  it('shouldTreatAsClick: small move is click, large move is drag', () => {
    expect(shouldTreatAsClick({ x: 0, y: 0 }, { x: 3, y: 4 })).toBe(true);
    expect(shouldTreatAsClick({ x: 0, y: 0 }, { x: 10, y: 0 })).toBe(false);
    expect(shouldTreatAsClick({ x: 5, y: 5 }, { x: 5, y: 5 })).toBe(true);
  });

  it('renders zoom controls', () => {
    renderGraph();
    expect(screen.getByLabelText('wiki.graphZoomIn')).toBeTruthy();
    expect(screen.getByLabelText('wiki.graphZoomOut')).toBeTruthy();
    expect(screen.getByLabelText('wiki.graphReset')).toBeTruthy();
  });
});
