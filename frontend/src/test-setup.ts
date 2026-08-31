import { vi } from 'vitest';

vi.mock('sonner', () => ({
  Toaster: () => null,
  toast: {
    success: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
    warning: vi.fn(),
    message: vi.fn(),
  },
}));

import i18n from '@/i18n';

await i18n.changeLanguage('en');

/**
 * Custom Canvas 2D mock for jsdom test environment.
 *
 * jsdom does not implement canvas pixel rendering, so getImageData() returns
 * all-zero pixels. This mock intercepts fillText/measureText/getImageData to
 * produce meaningful alpha-channel data based on the drawn text, allowing
 * particle-sampling functions to be tested.
 */

// ---- Minimal TextMetrics ---------------------------------------------------
class MockTextMetrics implements TextMetrics {
  readonly width: number;
  readonly actualBoundingBoxLeft = 0;
  readonly actualBoundingBoxRight = 0;
  readonly fontBoundingBoxAscent = 0;
  readonly fontBoundingBoxDescent = 0;
  readonly actualBoundingBoxAscent = 0;
  readonly actualBoundingBoxDescent = 0;
  readonly emHeightAscent = 0;
  readonly emHeightDescent = 0;
  readonly hangingBaseline = 0;
  readonly alphabeticBaseline = 0;
  readonly ideographicBaseline = 0;

  constructor(text: string, fontSizePx: number) {
    // Approximate text width: each character ~0.6 of fontSize
    this.width = text.length * fontSizePx * 0.6;
  }
}

// ---- Minimal ImageData -----------------------------------------------------
class MockImageData {
  readonly data: Uint8ClampedArray<ArrayBuffer>;
  readonly width: number;
  readonly height: number;
  readonly colorSpace: PredefinedColorSpace = 'srgb';

  constructor(width: number, height: number) {
    this.width = width;
    this.height = height;
    this.data = new Uint8ClampedArray(width * height * 4);
  }
}

// ---- Minimal CanvasRenderingContext2D mock ----------------------------------
class MockContext {
  private _canvas: HTMLCanvasElement;
  private _font = '10px sans-serif';
  private _fillStyle: string | CanvasGradient | CanvasPattern = '#000';
  private _strokeStyle: string | CanvasGradient | CanvasPattern = '#000';
  private _textBaseline: CanvasTextBaseline = 'alphabetic';
  private _textAlign: CanvasTextAlign = 'start';
  private _globalAlpha = 1;
  private _globalCompositeOperation: GlobalCompositeOperation = 'source-over';

  /** Track fillText calls so getImageData can produce sensible pixel data. */
  private _drawnTexts: Array<{
    text: string;
    x: number;
    y: number;
    font: string;
    fillStyle: string;
  }> = [];

  constructor(canvas: HTMLCanvasElement) {
    this._canvas = canvas;
  }

  get canvas(): HTMLCanvasElement {
    return this._canvas;
  }

  get font(): string {
    return this._font;
  }
  set font(v: string) {
    this._font = v;
  }

  get fillStyle(): string | CanvasGradient | CanvasPattern {
    return this._fillStyle;
  }
  set fillStyle(v: string | CanvasGradient | CanvasPattern) {
    this._fillStyle = v;
  }

  get strokeStyle(): string | CanvasGradient | CanvasPattern {
    return this._strokeStyle;
  }
  set strokeStyle(v: string | CanvasGradient | CanvasPattern) {
    this._strokeStyle = v;
  }

  get textBaseline(): CanvasTextBaseline {
    return this._textBaseline;
  }
  set textBaseline(v: CanvasTextBaseline) {
    this._textBaseline = v;
  }

  get textAlign(): CanvasTextAlign {
    return this._textAlign;
  }
  set textAlign(v: CanvasTextAlign) {
    this._textAlign = v;
  }

  get globalAlpha(): number {
    return this._globalAlpha;
  }
  set globalAlpha(v: number) {
    this._globalAlpha = v;
  }

  get globalCompositeOperation(): GlobalCompositeOperation {
    return this._globalCompositeOperation;
  }
  set globalCompositeOperation(v: GlobalCompositeOperation) {
    this._globalCompositeOperation = v;
  }

  // ---- Methods needed by sampleTextParticles -----------------------------

  measureText(text: string): TextMetrics {
    // Parse fontSize from font string (e.g. "700 24px sans-serif")
    const match = this._font.match(/(\d+(?:\.\d+)?)\s*px/);
    const size = match ? Number.parseFloat(match[1]) : 10;
    return new MockTextMetrics(text, size);
  }

  fillText(text: string, x: number, y: number): void {
    this._drawnTexts.push({
      text,
      x,
      y,
      font: this._font,
      fillStyle: String(this._fillStyle),
    });
  }

  getImageData(_x: number, _y: number, w: number, h: number): ImageData {
    const imageData = new MockImageData(w, h);
    const { data } = imageData;

    for (const drawn of this._drawnTexts) {
      // Parse font size
      const match = drawn.font.match(/(\d+(?:\.\d+)?)\s*px/);
      const fontSize = match ? Number.parseFloat(match[1]) : 10;

      // Approximate per-character dimensions
      const charW = fontSize * 0.5;
      const charH = fontSize * 0.85;
      const startX = drawn.x;
      // Rough vertical center given textBaseline='middle' or default
      const startY = drawn.y - charH / 2;

      for (let ci = 0; ci < drawn.text.length; ci++) {
        const baseX = Math.round(startX + ci * charW);
        const baseY = Math.round(startY);

        // Fill a rectangular "pixel blob" per character
        for (let py = 0; py < Math.ceil(charH); py++) {
          for (let px = 0; px < Math.ceil(charW); px++) {
            const sx = baseX + px;
            const sy = baseY + py;
            if (sx >= 0 && sx < w && sy >= 0 && sy < h) {
              const idx = (sy * w + sx) * 4 + 3; // alpha channel
              data[idx] = 255;
            }
          }
        }
      }
    }

    return imageData as unknown as ImageData;
  }

  // ---- Everything else is a no-op -----------------------------------------
  save(): void {}
  restore(): void {}
  beginPath(): void {}
  closePath(): void {}
  moveTo(_x: number, _y: number): void {}
  lineTo(_x: number, _y: number): void {}
  bezierCurveTo(_cp1x: number, _cp1y: number, _cp2x: number, _cp2y: number, _x: number, _y: number): void {}
  quadraticCurveTo(_cpx: number, _cpy: number, _x: number, _y: number): void {}
  arc(_x: number, _y: number, _r: number, _start: number, _end: number, _ccw?: boolean): void {}
  arcTo(_x1: number, _y1: number, _x2: number, _y2: number, _r: number): void {}
  rect(_x: number, _y: number, _w: number, _h: number): void {}
  fill(_fillRule?: CanvasFillRule): void {}
  stroke(): void {}
  clip(_fillRule?: CanvasFillRule): void {}
  clearRect(_x: number, _y: number, _w: number, _h: number): void {}
  fillRect(_x: number, _y: number, _w: number, _h: number): void {}
  strokeRect(_x: number, _y: number, _w: number, _h: number): void {}
  scale(_x: number, _y: number): void {}
  rotate(_a: number): void {}
  translate(_x: number, _y: number): void {}
  transform(_a: number, _b: number, _c: number, _d: number, _e: number, _f: number): void {}
  setTransform(
    _a?: number | DOMMatrix2DInit,
    _b?: number,
    _c?: number,
    _d?: number,
    _e?: number,
    _f?: number,
  ): void {}
  resetTransform(): void {}
  createLinearGradient(_x0: number, _y0: number, _x1: number, _y1: number): CanvasGradient {
    return { addColorStop: () => {} } as unknown as CanvasGradient;
  }
  createRadialGradient(
    _x0: number, _y0: number, _r0: number,
    _x1: number, _y1: number, _r1: number,
  ): CanvasGradient {
    return { addColorStop: () => {} } as unknown as CanvasGradient;
  }
  createPattern(_image: CanvasImageSource, _repetition: string | null): CanvasPattern | null {
    return null;
  }
  createImageData(_sw: number, _sh: number): ImageData;
  createImageData(_imagedata: ImageData): ImageData;
  createImageData(sw: number | ImageData, sh?: number): ImageData {
    if (typeof sw === 'number') {
      return new MockImageData(sw, sh ?? 0) as unknown as ImageData;
    }
    return new MockImageData(sw.width, sw.height) as unknown as ImageData;
  }
  putImageData(
    _imageData: ImageData, _dx: number, _dy: number,
    _dirtyX?: number, _dirtyY?: number, _dirtyWidth?: number, _dirtyHeight?: number,
  ): void {}
  isPointInPath(_x: number, _y: number, _fillRule?: CanvasFillRule): boolean;
  isPointInPath(_path: Path2D, _x: number, _y: number, _fillRule?: CanvasFillRule): boolean;
  isPointInPath(..._args: unknown[]): boolean {
    return false;
  }
  isPointInStroke(_x: number, _y: number): boolean;
  isPointInStroke(_path: Path2D, _x: number, _y: number): boolean;
  isPointInStroke(..._args: unknown[]): boolean {
    return false;
  }
  strokeText(_text: string, _x: number, _y: number, _maxWidth?: number): void {}
  drawImage(
    _image: CanvasImageSource, _sx: number, _sy: number,
    _sw?: number, _sh?: number, _dx?: number, _dy?: number,
    _dw?: number, _dh?: number,
  ): void {}
  setLineDash(_segments: number[]): void {}
  getLineDash(): number[] {
    return [];
  }
  getTransform(): DOMMatrix {
    return { a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 } as unknown as DOMMatrix;
  }
  getContextAttributes(): CanvasRenderingContext2DSettings {
    return { alpha: true, willReadFrequently: true };
  }
  roundRect(_x: number, _y: number, _w: number, _h: number, _radii?: number | DOMPointInit | (number | DOMPointInit)[]): void {}
  // Legacy Canvas 2D methods
  get lineWidth(): number { return 1; }
  set lineWidth(_v: number) {}
  get lineCap(): CanvasLineCap { return 'butt'; }
  set lineCap(_v: CanvasLineCap) {}
  get lineJoin(): CanvasLineJoin { return 'miter'; }
  set lineJoin(_v: CanvasLineJoin) {}
  get miterLimit(): number { return 10; }
  set miterLimit(_v: number) {}
  get shadowOffsetX(): number { return 0; }
  set shadowOffsetX(_v: number) {}
  get shadowOffsetY(): number { return 0; }
  set shadowOffsetY(_v: number) {}
  get shadowBlur(): number { return 0; }
  set shadowBlur(_v: number) {}
  get shadowColor(): string { return 'rgba(0, 0, 0, 0)'; }
  set shadowColor(_v: string) {}
  get filter(): string { return 'none'; }
  set filter(_v: string) {}
  get imageSmoothingEnabled(): boolean { return true; }
  set imageSmoothingEnabled(_v: boolean) {}
  get imageSmoothingQuality(): ImageSmoothingQuality { return 'low'; }
  set imageSmoothingQuality(_v: ImageSmoothingQuality) {}
  get direction(): CanvasDirection { return 'ltr'; }
  set direction(_v: CanvasDirection) {}

  // OffscreenCanvas support
  commit(): void {}
}

// Radix Select calls Element.scrollIntoView internally; jsdom lacks it — stub to no-op.
if (typeof Element !== 'undefined' && !Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function () {};
}

// ---- Hook into HTMLCanvasElement (only in jsdom environment) -----------------
if (typeof HTMLCanvasElement !== 'undefined') {
  const origGetContext = HTMLCanvasElement.prototype.getContext;

  HTMLCanvasElement.prototype.getContext = function (
    this: HTMLCanvasElement,
    contextId: string,
    _options?: unknown,
  ): CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D | ImageBitmapRenderingContext | WebGLRenderingContext | WebGL2RenderingContext | null {
    if (contextId === '2d') {
      return new MockContext(this) as unknown as CanvasRenderingContext2D;
    }
    // Fall back to original for other context types (e.g. webgl)
    return origGetContext.call(this, contextId as Parameters<typeof origGetContext>[0]);
  } as unknown as HTMLCanvasElement['getContext'];
}

// ---- Expose for vitest-canvas-mock compatible API --------------------------
export {};
