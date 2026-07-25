import { useEffect, useRef } from 'react';
import * as THREE from 'three';
import { useTheme } from '@/theme/ThemeProvider';

interface AuroraBackgroundProps {
  mode: 'fullscreen' | 'contained';
}

function isMobile(): boolean {
  if (typeof window === 'undefined') return false;
  return (
    window.matchMedia('(max-width: 768px)').matches ||
    (typeof navigator !== 'undefined' && (navigator.hardwareConcurrency ?? 8) < 4)
  );
}

function webGLSupported(): boolean {
  try {
    const canvas = document.createElement('canvas');
    return !!(canvas.getContext('webgl2') || canvas.getContext('webgl'));
  } catch {
    return false;
  }
}

const vertexShader = `
varying vec2 vUv;

void main() {
  vUv = uv;
  gl_Position = vec4(position, 1.0);
}
`;

const fragmentShader = `
uniform float uTime;
uniform vec2 uResolution;
uniform float uColorMode;
uniform float uIntensity;

varying vec2 vUv;

float hash(float n) {
  return fract(sin(n) * 43758.5453123);
}

float noise(vec3 x) {
  vec3 p = floor(x);
  vec3 f = fract(x);
  f = f * f * (3.0 - 2.0 * f);
  float n = p.x + p.y * 57.0 + 113.0 * p.z;
  return mix(
    mix(mix(hash(n + 0.0), hash(n + 1.0), f.x),
        mix(hash(n + 57.0), hash(n + 58.0), f.x), f.y),
    mix(mix(hash(n + 113.0), hash(n + 114.0), f.x),
        mix(hash(n + 170.0), hash(n + 171.0), f.x), f.y),
    f.z
  );
}

float fbm(vec3 p) {
  float value = 0.0;
  float amplitude = 0.5;
  float frequency = 1.0;
  for (int i = 0; i < 4; i++) {
    value += amplitude * noise(p * frequency);
    frequency *= 2.0;
    amplitude *= 0.5;
  }
  return value;
}

// 五段高度渐变：亮绿核心 → 青绿 → 蓝紫 → 紫罗兰 → 品红消散
vec3 gradientColor(float t, vec3 c0, vec3 c1, vec3 c2, vec3 c3, vec3 c4) {
  vec3 col = mix(c0, c1, smoothstep(0.00, 0.18, t));
  col = mix(col, c2, smoothstep(0.18, 0.45, t));
  col = mix(col, c3, smoothstep(0.45, 0.70, t));
  col = mix(col, c4, smoothstep(0.70, 1.00, t));
  return col;
}

vec3 auroraGradient(float t, float mode) {
  vec3 dark = gradientColor(
    t,
    vec3(0.35, 1.00, 0.55),
    vec3(0.10, 0.85, 0.60),
    vec3(0.25, 0.45, 0.95),
    vec3(0.60, 0.25, 0.90),
    vec3(0.85, 0.30, 0.65)
  );
  vec3 light = gradientColor(
    t,
    vec3(0.30, 0.85, 0.55),
    vec3(0.25, 0.75, 0.62),
    vec3(0.45, 0.50, 0.90),
    vec3(0.65, 0.42, 0.88),
    vec3(0.88, 0.48, 0.65)
  );
  return mix(dark, light, mode);
}

// 网格哈希星点，带闪烁
float starField(vec2 p, float t) {
  vec2 g = p * 90.0;
  vec2 cell = floor(g);
  float rnd = hash(cell.x * 127.1 + cell.y * 311.7);
  if (rnd < 0.92) return 0.0;
  vec2 sp = cell + vec2(
    hash(cell.x * 269.5 + cell.y * 183.3),
    hash(cell.x * 419.2 + cell.y * 371.9)
  );
  vec2 d = g - sp;
  float bright = (rnd - 0.92) / 0.08;
  float twinkle = 0.6 + 0.4 * sin(t * (1.0 + bright * 3.0) + rnd * 40.0);
  return exp(-dot(d, d) * 25.0) * bright * twinkle;
}

// 单层极光帘幕，返回 (rgb, intensity)
vec4 auroraLayer(vec2 p, float t, float layer, float mode) {
  float baseH, amp, freq, speed, seed, rayFreq, raySpeed, height, alpha;
  if (layer < 0.5) {
    // 近层：主帘幕
    baseH = 0.30; amp = 0.15; freq = 1.1; speed = 1.0; seed = 3.7;
    rayFreq = 7.0; raySpeed = 0.10; height = 0.50; alpha = 0.85;
  } else {
    // 远层：更高更淡，提供纵深
    baseH = 0.44; amp = 0.13; freq = 0.7; speed = 0.6; seed = 11.3;
    rayFreq = 4.0; raySpeed = 0.06; height = 0.55; alpha = 0.45;
  }

  float tt = t * speed;
  // 整帘随高度轻微摇摆
  float sway = 0.10 * sin(tt * 0.12 + p.y * 2.5 + seed);
  float x = p.x + sway;

  // 帘幕下缘：大幅波浪 + 细碎涟漪
  float e = baseH + amp * (fbm(vec3(x * freq + tt * 0.020, seed, tt * 0.05)) - 0.5) * 2.0;
  e += 0.025 * (fbm(vec3(x * 6.0 + tt * 0.03, seed * 5.0, tt * 0.08)) - 0.5) * 2.0;
  float d = p.y - e; // 距下缘高度
  if (d < -0.02 || d > height) return vec4(0.0);

  float hn = clamp(d / height, 0.0, 1.0);

  // 垂直光柱：只随 x 变化的噪声，高对比度
  float rn = fbm(vec3(x * rayFreq + tt * raySpeed, seed * 3.1, tt * 0.03));
  float rays = pow(clamp((rn - 0.30) * 2.4, 0.0, 1.0), 1.8);
  rays *= 1.0 - 0.35 * hn; // 顶部光柱略消散

  // 垂直轮廓：下缘亮边 + 向上指数衰减的帘体
  float rim = exp(-d * 8.0) * 1.25;
  float body = exp(-hn * 1.9) * 0.85;
  float profile = (rim + body) * smoothstep(-0.02, 0.015, d);
  profile *= 1.0 - smoothstep(0.7, 1.0, hn);

  float inten = profile * (0.12 + 0.88 * rays) * alpha;
  return vec4(auroraGradient(hn, mode), inten);
}

void main() {
  float aspect = uResolution.x / uResolution.y;
  vec2 p = vec2(vUv.x * aspect, vUv.y);

  // 夜空：深蓝渐变 + 地平线微光
  vec3 darkSky = mix(vec3(0.030, 0.045, 0.085), vec3(0.012, 0.010, 0.045), vUv.y);
  darkSky += exp(-vUv.y * 5.0) * 0.05 * vec3(0.4, 0.5, 0.9);
  vec3 lightSky = mix(vec3(0.93, 0.95, 1.00), vec3(0.86, 0.90, 0.98), vUv.y);

  vec3 darkCol = darkSky;
  vec3 lightCol = lightSky;

  // 星点（仅暗色），地平线附近淡出
  float s = starField(p, uTime) * smoothstep(0.15, 0.55, vUv.y) * uIntensity;
  darkCol += s * vec3(0.8, 0.85, 1.0);

  // 极光：暗色下加法发光；亮色下 alpha 混合（白色背景上加法无法显色）
  for (int i = 1; i >= 0; i--) {
    vec4 a = auroraLayer(p, uTime, float(i), uColorMode);
    a.a *= uIntensity;
    darkCol += a.rgb * a.a;
    float blend = clamp(a.a * 1.1, 0.0, 0.75);
    lightCol = mix(lightCol, a.rgb, blend);
  }

  // 暗色软色调映射，避免过曝硬切
  darkCol = 1.0 - exp(-darkCol * 1.15);

  // 轻量暗角（亮色模式几乎不可见）
  vec2 c = vUv - 0.5;
  float dd = dot(c, c);
  darkCol *= 1.0 - dd * 0.55;
  lightCol *= 1.0 - dd * 0.15;

  gl_FragColor = vec4(mix(darkCol, lightCol, uColorMode), 1.0);
}
`;

export default function AuroraBackground({ mode }: AuroraBackgroundProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const { resolvedTheme } = useTheme();
  const colorModeTargetRef = useRef(resolvedTheme === 'dark' ? 0.0 : 1.0);
  const modeRef = useRef(mode);

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
      // contained 模式（Header）整体降低极光与星点强度，避免干扰导航文字
      uIntensity: { value: modeRef.current === 'fullscreen' ? 1.0 : 0.6 },
    };

    // Shader material
    const material = new THREE.ShaderMaterial({
      uniforms,
      vertexShader,
      fragmentShader,
    });

    // Scene setup
    const scene = new THREE.Scene();
    const mesh = new THREE.Mesh(geometry, material);
    scene.add(mesh);

    // Orthographic camera for NDC rendering
    const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 10);
    camera.position.z = 1;

    // Renderer
    const mobileCheck = isMobile();
    const renderer = new THREE.WebGLRenderer({ antialias: !mobileCheck, alpha: false });
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

  const containerClasses =
    mode === 'fullscreen'
      ? 'fixed inset-0 pointer-events-none overflow-hidden'
      : 'absolute inset-0 pointer-events-none overflow-hidden';

  return (
    <div
      ref={containerRef}
      className={containerClasses}
      aria-hidden="true"
      style={{ zIndex: 0 }}
    />
  );
}
