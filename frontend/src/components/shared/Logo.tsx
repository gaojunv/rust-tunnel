import { useEffect, useState } from 'react';
import { cn } from '@/lib/utils';

interface LogoProps {
  className?: string;
}

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () =>
      typeof window !== 'undefined' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener?.('change', onChange);
    return () => mq.removeEventListener?.('change', onChange);
  }, []);
  return reduced;
}

/**
 * 品牌 Logo：渐变圆角方块 + 「隧道入口」图形（同心圆隧道 + 彗星流光射入）。
 * 与 public/logo.svg（favicon）保持同一视觉。
 * 彗星小光点沿外圈环绕后射入隧道中心（SMIL animateMotion），
 * 遵循 prefers-reduced-motion（退化为静态 logo）。
 */
export function Logo({ className }: LogoProps) {
  const reducedMotion = usePrefersReducedMotion();
  return (
    <svg viewBox="0 0 48 48" className={cn('h-7 w-7', className)} aria-hidden="true">
      <defs>
        <linearGradient id="logo-gradient" x1="0" y1="0" x2="48" y2="48" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#6366f1" />
          <stop offset="0.55" stopColor="#3b82f6" />
          <stop offset="1" stopColor="#06b6d4" />
        </linearGradient>
        <radialGradient id="logo-glow" cx="0.5" cy="0.35" r="0.9">
          <stop offset="0" stopColor="#ffffff" stopOpacity="0.28" />
          <stop offset="0.6" stopColor="#ffffff" stopOpacity="0" />
        </radialGradient>
        <linearGradient id="logo-tail" x1="5" y1="27.5" x2="20" y2="24.5" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#ffffff" stopOpacity="0" />
          <stop offset="1" stopColor="#ffffff" stopOpacity="0.95" />
        </linearGradient>
        {/* 彗星运行轨道：沿外圈圆环（与隧道外圆同圆心同半径） */}
        <path
          id="logo-comet-orbit"
          d="M 38.5 24 A 11 11 0 1 1 27.5 13"
          fill="none"
        />
      </defs>
      <rect width="48" height="48" rx="12" fill="url(#logo-gradient)" />
      <rect width="48" height="48" rx="12" fill="url(#logo-glow)" />
      <g fill="none" stroke="#ffffff" strokeWidth="3" strokeLinecap="round">
        <circle cx="27.5" cy="24" r="11" opacity="0.4" />
        <circle cx="27.5" cy="24" r="6.5" opacity="0.75" />
      </g>
      <circle cx="27.5" cy="24" r="2.6" fill="#ffffff" />
      <path
        d="M5 27.5 Q 12 25.5 19 24.6"
        fill="none"
        stroke="url(#logo-tail)"
        strokeWidth="2.6"
        strokeLinecap="round"
      />
      <circle cx="19.4" cy="24.5" r="1.8" fill="#ffffff" />
      {!reducedMotion && (
        <circle r="1.8" fill="#ffffff">
          <animateMotion dur="3s" repeatCount="indefinite" rotate="auto">
            <mpath href="#logo-comet-orbit" />
          </animateMotion>
          <animate
            attributeName="opacity"
            values="0;1;1;0"
            keyTimes="0;0.15;0.75;1"
            dur="3s"
            repeatCount="indefinite"
          />
        </circle>
      )}
    </svg>
  );
}
