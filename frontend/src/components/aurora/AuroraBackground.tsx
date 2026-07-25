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
  for (int i = 0; i < 3; i++) {
    value += amplitude * noise(p * frequency);
    frequency *= 2.0;
    amplitude *= 0.5;
  }
  return value;
}

void main() {
  vec2 uv = vUv;
  float aspect = uResolution.x / uResolution.y;
  vec2 pos = uv * 2.0 - 1.0;
  pos.x *= aspect;

  // Sky gradient
  vec3 darkSkyTop = vec3(0.008, 0.004, 0.035);
  vec3 darkSkyBottom = vec3(0.035, 0.015, 0.065);
  vec3 lightSkyTop = vec3(0.82, 0.78, 0.88);
  vec3 lightSkyBottom = vec3(0.90, 0.88, 0.94);

  vec3 darkSky = mix(darkSkyBottom, darkSkyTop, uv.y);
  vec3 lightSky = mix(lightSkyBottom, lightSkyTop, uv.y);
  vec3 skyColor = mix(darkSky, lightSky, uColorMode);

  // Aurora band colors: dark mode (high sat) vs light mode (low sat)
  vec3 darkBands[3];
  darkBands[0] = vec3(0.40, 0.12, 0.85); // blue-purple
  darkBands[1] = vec3(0.05, 0.72, 0.48); // mint-green
  darkBands[2] = vec3(0.85, 0.25, 0.35); // sunset-red

  vec3 lightBands[3];
  lightBands[0] = vec3(0.70, 0.58, 0.90); // light purple
  lightBands[1] = vec3(0.58, 0.78, 0.68); // light mint
  lightBands[2] = vec3(0.88, 0.68, 0.60); // warm peach

  vec3 auroraColor = vec3(0.0);
  float totalAlpha = 0.0;

  for (int i = 0; i < 3; i++) {
    float fi = float(i);

    // Vertical position with sinusoidal drift
    float yDrift = 0.18 * sin(uTime * 0.10 + fi * 2.094 + pos.x * 0.4);
    float yBase = -0.25 + fi * 0.28;
    float yCenter = yBase + yDrift;

    // Aurora curtain shape using FBM noise
    vec3 noiseCoord = vec3(
      pos.x * 1.8 + uTime * 0.025,
      uv.y * 2.5 + uTime * 0.018 + fi * 0.5,
      fi * 7.0 + uTime * 0.008
    );
    float curtain = fbm(noiseCoord);

    // Vertical falloff from band center
    float yDist = abs(pos.y - yCenter);
    float verticalFade = smoothstep(0.8, 0.05, yDist);

    float alpha = verticalFade * curtain * 0.7;
    vec3 bandColor = mix(darkBands[i], lightBands[i], uColorMode);
    auroraColor += bandColor * alpha;
    totalAlpha += alpha;
  }

  // Normalize to prevent over-saturation when bands overlap
  if (totalAlpha > 0.0) {
    auroraColor /= max(totalAlpha, 1.0);
  }

  // Composite aurora over sky
  float mixFactor = min(totalAlpha, 1.0) * 0.85;
  vec3 finalColor = mix(skyColor, auroraColor, mixFactor);

  // Edge fog (vignette) - smoothstep at 4 edges
  float fogMargin = 0.12;
  float edgeFog = 1.0;
  edgeFog *= smoothstep(0.0, fogMargin, uv.x);
  edgeFog *= smoothstep(0.0, fogMargin, uv.y);
  edgeFog *= smoothstep(1.0, 1.0 - fogMargin, uv.x);
  edgeFog *= smoothstep(1.0, 1.0 - fogMargin, uv.y);

  finalColor *= edgeFog;

  gl_FragColor = vec4(finalColor, 1.0);
}
`;

export default function AuroraBackground({ mode }: AuroraBackgroundProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const materialRef = useRef<THREE.ShaderMaterial | null>(null);
  const { resolvedTheme } = useTheme();

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
      uColorMode: { value: resolvedTheme === 'dark' ? 0.0 : 1.0 },
    };

    // Shader material
    const material = new THREE.ShaderMaterial({
      uniforms,
      vertexShader,
      fragmentShader,
    });
    materialRef.current = material;

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

      materialRef.current = null;
    };
  }, []);

  // Update color mode uniform when theme changes (without re-creating WebGL)
  useEffect(() => {
    if (materialRef.current) {
      materialRef.current.uniforms.uColorMode.value = resolvedTheme === 'dark' ? 0.0 : 1.0;
    }
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
