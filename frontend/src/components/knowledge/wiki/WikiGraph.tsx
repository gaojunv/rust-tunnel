import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from 'd3-force';
import { Loader2, Network, AlertTriangle } from 'lucide-react';
import type { WikiGraphEdge, WikiPageSummary } from '@/types';

/** 力导向图谱（d3-force + 自绘 SVG）。
 *  - 悬空边（dangling，目标页不存在）红色虚线，从源节点向一个由 to_ref 哈希决定
 *    的方向延伸（不入力导向布局，不参与力计算）。
 *  - locked 节点描边加粗/异色。
 *  - 交互：滚轮缩放 + 空白拖拽平移 + 节点拖拽固定（fx/fy 钉住）。
 *  - jsdom（测试）退化为静态网格布局：不做 d3 布局计算，仍渲染节点/边以覆盖测试。
 */
const VIEW_W = 800;
const VIEW_H = 600;
const NODE_R = 12;
const COLLIDE_R = NODE_R + 6;
const MAX_NODES = 500;
const DANGLE_LEN = 90;

/** 环境退化探测（照 CodeMirrorEditor.isEditorSupported 先例）。 */
function isJsdom(): boolean {
  return typeof navigator !== 'undefined' && /jsdom/i.test(navigator.userAgent);
}

/** 确定性字符串哈希：dangling 边延伸方向/退化布局的抖动都靠它，保证多次渲染稳定。 */
function hashStr(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) {
    h = (h * 31 + s.charCodeAt(i)) | 0;
  }
  return Math.abs(h);
}

interface SimNode extends SimulationNodeDatum {
  id: string;
  ref: string;
  title: string;
  locked: boolean;
  use_count: number;
}

interface SimLink extends SimulationLinkDatum<SimNode> {
  source: string | SimNode;
  target: string | SimNode;
}

interface Props {
  nodes: WikiPageSummary[];
  edges: WikiGraphEdge[];
  loading?: boolean;
  /** 点击节点回调（ref）。与页面 Tab 联动。 */
  onNodeClick?: (ref: string) => void;
}

export default function WikiGraph({ nodes, edges, loading, onNodeClick }: Props) {
  const { t } = useTranslation();
  const degraded = isJsdom();

  // 节点数 >500：按 use_count 排序截断，超出部分提示降级。
  const limitedNodes = useMemo(() => {
    const sorted = [...nodes].sort((a, b) => b.use_count - a.use_count);
    return sorted.slice(0, MAX_NODES);
  }, [nodes]);
  const truncated = nodes.length > limitedNodes.length;
  const nodeIdSet = useMemo(
    () => new Set(limitedNodes.map((n) => n.id)),
    [limitedNodes],
  );
  // 源节点在渲染集内的边（跨出渲染集的边按悬空样式画成虚线）。
  const visibleEdges = useMemo(
    () => edges.filter((e) => nodeIdSet.has(e.from)),
    [edges, nodeIdSet],
  );

  // ── 布局：正常模式 = d3 力导向；退化模式 = 静态网格 ─────────────
  const [simNodes, setSimNodes] = useState<SimNode[] | null>(null);
  const [, forceRender] = useState(0);
  const simRef = useRef<Simulation<SimNode, SimLink> | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);

  // 平移缩放状态（退化模式禁用）
  const [view, setView] = useState({ x: 0, y: 0, scale: 1 });
  const dragRef = useRef<{ kind: 'pan' } | { kind: 'node'; id: string } | null>(null);
  const viewRef = useRef(view);
  viewRef.current = view;

  useEffect(() => {
    if (degraded) {
      setSimNodes(null);
      simRef.current?.stop();
      simRef.current = null;
      return;
    }
    // 节点初始散布在中心附近，避免全部重叠在 (0,0)
    const base: SimNode[] = limitedNodes.map((n) => ({
      id: n.id,
      ref: n.ref,
      title: n.title,
      locked: n.locked,
      use_count: n.use_count,
      x: VIEW_W / 2 + ((hashStr(n.ref) % 120) - 60),
      y: VIEW_H / 2 + ((hashStr(n.id) % 120) - 60),
    }));
    const idToNode = new Map(base.map((n) => [n.id, n]));
    const links: SimLink[] = visibleEdges
      .filter((e) => !e.dangling && e.to && idToNode.has(e.to))
      .map((e) => ({ source: e.from, target: e.to! }));

    const sim = forceSimulation<SimNode>(base)
      .force('link', forceLink<SimNode, SimLink>(links).id((d) => d.id).distance(90).strength(0.5))
      .force('charge', forceManyBody().strength(-260))
      .force('center', forceCenter(VIEW_W / 2, VIEW_H / 2))
      .force('collide', forceCollide<SimNode>().radius(COLLIDE_R));
    sim.on('tick', () => forceRender((n) => n + 1));
    simRef.current = sim;
    setSimNodes(base);
    return () => {
      sim.stop();
      simRef.current = null;
    };
  }, [degraded, limitedNodes, visibleEdges]);

  // 退化模式：静态网格布局
  const degradedPos = useMemo(() => {
    const map = new Map<string, { x: number; y: number }>();
    const cols = Math.max(1, Math.floor(Math.sqrt(limitedNodes.length * (VIEW_W / VIEW_H))));
    const spacingX = limitedNodes.length > 1 ? (VIEW_W - 80) / Math.max(1, cols - 1) : 0;
    const rows = Math.max(1, Math.ceil(limitedNodes.length / cols));
    const spacingY = rows > 1 ? (VIEW_H - 80) / (rows - 1) : 0;
    limitedNodes.forEach((n, i) => {
      const col = i % cols;
      const row = Math.floor(i / cols);
      map.set(n.id, {
        x: 40 + (cols > 1 ? col * spacingX : 0),
        y: 40 + (rows > 1 ? row * spacingY : 0),
      });
    });
    return map;
  }, [limitedNodes]);

  const posOf = (id: string): { x: number; y: number } | undefined => {
    if (degraded) return degradedPos.get(id);
    const n = simNodes?.find((s) => s.id === id);
    if (!n) return undefined;
    return { x: n.x ?? 0, y: n.y ?? 0 };
  };

  // 悬空边/被截断边：从源节点向 to_ref 哈希方向延伸固定长度
  const dangleEnd = (e: WikiGraphEdge, src: { x: number; y: number }) => {
    const angle = (hashStr(e.to_ref) % 360) * (Math.PI / 180);
    return { x: src.x + Math.cos(angle) * DANGLE_LEN, y: src.y + Math.sin(angle) * DANGLE_LEN };
  };

  // ── 交互（退化模式跳过） ───────────────────────────────────────
  const toSvgPoint = (clientX: number, clientY: number) => {
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return { x: 0, y: 0 };
    const v = viewRef.current;
    return { x: (clientX - rect.left - v.x) / v.scale, y: (clientY - rect.top - v.y) / v.scale };
  };

  const startPan = (clientX: number, clientY: number) => {
    if (degraded) return;
    dragRef.current = { kind: 'pan' };
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return;
    // 记录拖拽起点（screen 坐标），移动时按 delta 平移
    (dragRef.current as { kind: 'pan'; sx: number; sy: number; ox: number; oy: number }).sx = clientX;
    (dragRef.current as { kind: 'pan'; sx: number; sy: number; ox: number; oy: number }).sy = clientY;
    const v = viewRef.current;
    (dragRef.current as { kind: 'pan'; sx: number; sy: number; ox: number; oy: number }).ox = v.x;
    (dragRef.current as { kind: 'pan'; sx: number; sy: number; ox: number; oy: number }).oy = v.y;
  };

  const startNodeDrag = (id: string, clientX: number, clientY: number) => {
    if (degraded) return;
    const node = simNodes?.find((s) => s.id === id);
    if (!node) return;
    const p = toSvgPoint(clientX, clientY);
    node.fx = p.x;
    node.fy = p.y;
    dragRef.current = { kind: 'node', id };
    simRef.current?.alphaTarget(0.3).restart();
  };

  const onMove = (clientX: number, clientY: number) => {
    const d = dragRef.current;
    if (!d || degraded) return;
    if (d.kind === 'pan') {
      const st = d as { kind: 'pan'; sx: number; sy: number; ox: number; oy: number };
      setView((v) => ({ ...v, x: st.ox + (clientX - st.sx), y: st.oy + (clientY - st.sy) }));
    } else {
      const node = simNodes?.find((s) => s.id === d.id);
      if (!node) return;
      const p = toSvgPoint(clientX, clientY);
      node.fx = p.x;
      node.fy = p.y;
      forceRender((n) => n + 1);
    }
  };

  const endDrag = () => {
    const d = dragRef.current;
    dragRef.current = null;
    if (!d || degraded) return;
    if (d.kind === 'node') {
      // 拖完钉住（fx/fy 保留），收敛到静止
      simRef.current?.alphaTarget(0).restart();
    }
  };

  const onWheel = (e: React.WheelEvent) => {
    if (degraded) return;
    e.preventDefault();
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return;
    const v = viewRef.current;
    const factor = e.deltaY < 0 ? 1.12 : 1 / 1.12;
    const newScale = Math.min(4, Math.max(0.2, v.scale * factor));
    // 保持光标下的世界点不动
    const cx = e.clientX - rect.left;
    const cy = e.clientY - rect.top;
    const wx = (cx - v.x) / v.scale;
    const wy = (cy - v.y) / v.scale;
    setView({ x: cx - wx * newScale, y: cy - wy * newScale, scale: newScale });
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center gap-2 p-10 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        {t('common.loading')}
      </div>
    );
  }

  if (nodes.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 p-10 text-sm text-muted-foreground">
        <Network className="h-8 w-8 opacity-40" />
        <span>{t('wiki.graphEmpty')}</span>
      </div>
    );
  }

  const renderEdges = visibleEdges.map((e, i) => {
    const src = posOf(e.from);
    if (!src) return null;
    const dst = !e.dangling && e.to ? posOf(e.to) : undefined;
    if (e.dangling || !dst) {
      // 悬空边或目标被截断：红色虚线延伸到源节点外的确定方向
      const end = dangleEnd(e, src);
      return (
        <g key={`d${i}`}>
          <line
            className="wiki-edge wiki-edge-dangling"
            x1={src.x}
            y1={src.y}
            x2={end.x}
            y2={end.y}
            strokeDasharray="6 4"
          />
          <text x={end.x + 4} y={end.y + 3} className="fill-destructive/70 text-[10px]">
            {e.to_ref}
          </text>
        </g>
      );
    }
    return (
      <line
        key={`e${i}`}
        className="wiki-edge"
        x1={src.x}
        y1={src.y}
        x2={dst.x}
        y2={dst.y}
      />
    );
  });

  const renderNodes = limitedNodes.map((n) => {
    const pos = posOf(n.id);
    if (!pos) return null;
    const showLabel = limitedNodes.length <= 120;
    return (
      <g
        key={n.id}
        data-ref={n.ref}
        className={n.locked ? 'wiki-node wiki-node-locked' : 'wiki-node'}
        onClick={(e) => {
          e.stopPropagation();
          onNodeClick?.(n.ref);
        }}
        onPointerDown={(e) => {
          e.stopPropagation();
          startNodeDrag(n.id, e.clientX, e.clientY);
        }}
        style={{ cursor: onNodeClick ? 'pointer' : 'grab' }}
      >
        <circle r={NODE_R} cx={pos.x} cy={pos.y} />
        <title>
          {n.ref}
          {n.title ? ` — ${n.title}` : ''}
        </title>
        {showLabel && (
          <text x={pos.x} y={pos.y + NODE_R + 12} textAnchor="middle" className="wiki-node-label">
            {n.ref}
          </text>
        )}
      </g>
    );
  });

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
        <div className="flex items-center gap-4">
          <span className="flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-full bg-primary/70" />
            {t('wiki.graphLegendNode')}
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block h-0.5 w-4 rounded bg-destructive" />
            {t('wiki.graphLegendDangling')}
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-full border-2 border-primary" />
            {t('wiki.graphLegendLocked')}
          </span>
        </div>
        {truncated && (
          <span className="flex items-center gap-1 text-amber-600 dark:text-amber-400">
            <AlertTriangle className="h-3.5 w-3.5" />
            {t('wiki.graphTooMany', { count: limitedNodes.length, total: nodes.length })}
          </span>
        )}
      </div>
      <div className="overflow-hidden rounded-lg border border-border bg-muted/20">
        <svg
          ref={svgRef}
          viewBox={`0 0 ${VIEW_W} ${VIEW_H}`}
          className="block h-auto w-full touch-none select-none"
          onWheel={onWheel}
          onPointerMove={(e) => onMove(e.clientX, e.clientY)}
          onPointerUp={endDrag}
          onPointerLeave={endDrag}
        >
          <defs>
            <style>{`
              .wiki-edge { stroke: hsl(var(--border)); stroke-width: 1.5; }
              .wiki-edge-dangling { stroke: hsl(var(--destructive) / 0.7); stroke-width: 1.5; }
              .wiki-node circle {
                fill: hsl(var(--card));
                stroke: hsl(var(--muted-foreground) / 0.5);
                stroke-width: 1.5;
                transition: filter 150ms;
              }
              .wiki-node:hover circle { filter: brightness(0.92); }
              .wiki-node-locked circle { stroke: hsl(var(--primary)); stroke-width: 3; }
              .wiki-node-label { fill: hsl(var(--muted-foreground)); font-size: 10px; }
            `}</style>
          </defs>
          <g transform={`translate(${view.x}, ${view.y}) scale(${view.scale})`}>
            {/* 背景：接收空白拖拽平移 */}
            <rect
              x={-200}
              y={-200}
              width={VIEW_W + 400}
              height={VIEW_H + 400}
              fill="transparent"
              onPointerDown={(e) => {
                if (e.button === 0) startPan(e.clientX, e.clientY);
              }}
            />
            {renderEdges}
            {renderNodes}
          </g>
        </svg>
      </div>
    </div>
  );
}
