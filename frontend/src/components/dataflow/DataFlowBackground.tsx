import { useEffect, useRef } from 'react';
import * as THREE from 'three';
import { useTheme } from '@/theme/ThemeProvider';

function webGLSupported(): boolean {
  try {
    const canvas = document.createElement('canvas');
    return !!(canvas.getContext('webgl2') || canvas.getContext('webgl'));
  } catch {
    return false;
  }
}

function isMobile(): boolean {
  if (typeof window === 'undefined') return false;
  return (
    window.matchMedia('(max-width: 768px)').matches ||
    (typeof navigator !== 'undefined' && (navigator.hardwareConcurrency ?? 8) < 4)
  );
}

const vertexShader = `
varying vec2 vUv;

void main() {
  vUv = uv;
  gl_Position = vec4(position, 1.0);
}
`;

// 数据流效果：多条水平信道，每条信道有微弱底线和两个反向/同向移动的
// 光脉冲（彗尾式拖尾），模拟隧道中双向流动的数据包。
// canvas 透明，叠加在 Header 的玻璃拟态背景之上。
const fragmentShader = `
uniform float uTime;
uniform vec2 uResolution;
uniform float uColorMode;

varying vec2 vUv;

float hash(float n) {
  return fract(sin(n) * 43758.5453123);
}

// 品牌色：暗色主题 --primary(199° 青蓝) / --chart-2(262° 紫)
const vec3 CYAN = vec3(0.10, 0.66, 0.87);
const vec3 VIOLET = vec3(0.55, 0.42, 0.91);

void main() {
  float h = uResolution.y;
  float x = vUv.x;
  float py = vUv.y;

  vec3 col = vec3(0.0);
  float alpha = 0.0;

  for (int i = 0; i < 6; i++) {
    float fi = float(i);
    float ly = 0.18 + 0.64 * hash(fi * 12.9898 + 3.7);      // 信道纵向位置
    float d = hash(fi * 78.233 + 1.3) > 0.42 ? 1.0 : -1.0;   // 流动方向
    float speed = 0.05 + 0.10 * hash(fi * 39.425 + 7.1);     // 屏宽/秒
    vec3 laneColor = hash(fi * 11.17 + 5.5) > 0.35 ? CYAN : VIOLET;
    float phase = hash(fi * 55.31 + 9.2);

    float dy = abs(py - ly);

    // 信道底线（约 1px，很淡）
    float la = exp(-dy * dy * (h * 1.6) * (h * 1.6)) * 0.10;

    // 每条信道两个脉冲，相位错开半个周期
    float pa = 0.0;
    for (int k = 0; k < 2; k++) {
      float ph = fract(uTime * speed + phase + float(k) * 0.5);
      float head = ph * 1.3 - 0.15;                          // 扫过 [-0.15, 1.15]
      if (d < 0.0) head = 1.0 - head;
      float behind = (head - x) * d;                         // 沿流向距脉冲头的距离
      float tail = behind > 0.0 ? exp(-behind * 18.0)        // 后方指数拖尾
                                : exp(behind * 60.0);        // 前方快速截止
      float glow = exp(-dy * dy * (h * 0.55) * (h * 0.55));
      float core = exp(-dy * dy * (h * 1.3) * (h * 1.3));
      pa = max(pa, tail * (glow * 0.45 + core * 0.85));
    }

    // 该信道的合成透明度，over 叠加到累计结果
    float laneA = clamp(la + pa, 0.0, 1.0);
    col = laneColor * laneA + col * (1.0 - laneA);
    alpha = laneA + alpha * (1.0 - laneA);
  }

  // 亮色模式：降透明度并略去饱和，避免白底上刺眼
  vec3 lightCol = vec3(
    mix(col.r, 0.30, 0.15),
    mix(col.g, 0.42, 0.15),
    mix(col.b, 0.62, 0.10)
  );
  col = mix(col, lightCol, uColorMode);
  alpha = mix(alpha, alpha * 0.70, uColorMode);

  gl_FragColor = vec4(col, alpha);
}
`;

export default function DataFlowBackground() {
  const containerRef = useRef<HTMLDivElement>(null);
  const { resolvedTheme } = useTheme();
  const colorModeTargetRef = useRef(resolvedTheme === 'dark' ? 0.0 : 1.0);

  // Initialize WebGL scene
  useEffect(() => {
    if (!webGLSupported()) return;
    const container = containerRef.current;
    if (!container) return;

    // Fullscreen quad geometry in NDC [-1, 1]
    const geometry = new THREE.BufferGeometry();
    const vertices = new Float32Array([-1, -1, 0, 1, -1, 0, 1, 1, 0, -1, 1, 0]);
    const uvs = new Float32Array([0, 0, 1, 0, 1, 1, 0, 1]);
    const indices = [0, 1, 2, 0, 2, 3];
    geometry.setAttribute('position', new THREE.BufferAttribute(vertices, 3));
    geometry.setAttribute('uv', new THREE.BufferAttribute(uvs, 2));
    geometry.setIndex(indices);

    // Uniforms
    const uniforms = {
      uTime: { value: 0.0 },
      uResolution: { value: new THREE.Vector2(container.clientWidth, container.clientHeight) },
      uColorMode: { value: colorModeTargetRef.current },
    };

    // Shader material
    const material = new THREE.ShaderMaterial({
      uniforms,
      vertexShader,
      fragmentShader,
      transparent: true,
    });

    // Scene setup
    const scene = new THREE.Scene();
    const mesh = new THREE.Mesh(geometry, material);
    scene.add(mesh);

    // Orthographic camera for NDC rendering
    const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 10);
    camera.position.z = 1;

    // Renderer: 透明画布，straight alpha（非预乘），与着色器输出一致
    const mobileCheck = isMobile();
    const renderer = new THREE.WebGLRenderer({
      antialias: !mobileCheck,
      alpha: true,
      premultipliedAlpha: false,
    });
    renderer.setClearColor(0x000000, 0);
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, mobileCheck ? 1 : 2));
    renderer.setSize(container.clientWidth, container.clientHeight);
    container.appendChild(renderer.domElement);

    // Animation state
    const pausedRef = { current: false };
    let lastFrameTime = performance.now();
    let animationId: number | null = null;

    const animate = () => {
      if (!document.hidden) {
        const now = performance.now();
        const delta = (now - lastFrameTime) / 1000;
        lastFrameTime = now;

        if (!pausedRef.current) {
          uniforms.uTime.value += delta;
        }

        // 主题切换时 uColorMode 向目标值平滑插值（约 1 秒过渡）
        const cm = uniforms.uColorMode;
        cm.value += (colorModeTargetRef.current - cm.value) * Math.min(1, delta * 3.0);

        renderer.render(scene, camera);
        animationId = requestAnimationFrame(animate);
      } else {
        animationId = null;
      }
    };

    animationId = requestAnimationFrame(animate);

    // Resize handling
    const handleResize = () => {
      const w = container.clientWidth;
      const h = container.clientHeight;

      if (w > 0 && h > 0) {
        renderer.setSize(w, h);
        uniforms.uResolution.value.set(w, h);
      }
    };

    const resizeObserver = new ResizeObserver(handleResize);
    resizeObserver.observe(container);

    // Visibility handling
    const handleVisibilityChange = () => {
      if (document.hidden) {
        if (animationId !== null) {
          cancelAnimationFrame(animationId);
          animationId = null;
        }
      } else {
        lastFrameTime = performance.now();
        animationId = requestAnimationFrame(animate);
      }
    };

    document.addEventListener('visibilitychange', handleVisibilityChange);

    // Reduced motion handling
    const reducedMotionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    pausedRef.current = reducedMotionQuery.matches;

    const handleReducedMotionChange = (e: MediaQueryListEvent) => {
      pausedRef.current = e.matches;
    };

    reducedMotionQuery.addEventListener('change', handleReducedMotionChange);

    // Cleanup
    return () => {
      if (animationId !== null) {
        cancelAnimationFrame(animationId);
      }

      document.removeEventListener('visibilitychange', handleVisibilityChange);
      reducedMotionQuery.removeEventListener('change', handleReducedMotionChange);
      resizeObserver.disconnect();

      renderer.dispose();
      geometry.dispose();
      material.dispose();

      if (container.contains(renderer.domElement)) {
        container.removeChild(renderer.domElement);
      }
    };
  }, []);

  // 主题变化只更新目标值，由渲染循环平滑过渡 uColorMode（不重建 WebGL）
  useEffect(() => {
    colorModeTargetRef.current = resolvedTheme === 'dark' ? 0.0 : 1.0;
  }, [resolvedTheme]);

  return (
    <div
      ref={containerRef}
      className="absolute inset-0 pointer-events-none overflow-hidden"
      aria-hidden="true"
      style={{ zIndex: 0 }}
    />
  );
}
