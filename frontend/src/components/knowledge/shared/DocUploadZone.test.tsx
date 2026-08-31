// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import DocUploadZone, { TEXT_MAX_BYTES, BINARY_MAX_BYTES } from './DocUploadZone';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, opts?: Record<string, unknown>) => {
    if (opts && typeof opts === 'object' && 'name' in opts) return `${k} ${(opts as Record<string,string>).name}`;
    if (k === 'ks.uploadReasonExt') return 'ks.uploadReasonExt';
    if (k === 'ks.uploadReasonSize') return `ks.uploadReasonSize ${(opts as Record<string,string>)?.max ?? ''}`;
    if (k === 'ks.uploading') return 'ks.uploading';
    if (k === 'ks.uploadInvalidFile') {
      const o = opts as Record<string,string>;
      return `ks.uploadInvalidFile ${o.name} ${o.reason}`;
    }
    return k;
  } }),
}));

const labels = {
  uploadHint: 'drop here',
  browse: 'browse files',
  fileInvalid: 'invalid file',
};

function makeFile(name: string, size: number, type = 'text/plain'): File {
  const f = new File(['x'], name, { type });
  Object.defineProperty(f, 'size', { value: size });
  return f;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('DocUploadZone', () => {
  it('renders hint and browse labels', () => {
    render(<DocUploadZone labels={labels} onUpload={vi.fn()} />);
    expect(screen.getByText('drop here')).toBeTruthy();
    expect(screen.getByText('browse files')).toBeTruthy();
  });

  it('shows uploading spinner when isUploading', () => {
    const { container } = render(<DocUploadZone labels={labels} onUpload={vi.fn()} isUploading />);
    expect(container.querySelector('.animate-spin')).toBeTruthy();
  });

  it('rejects oversized text file and shows per-file error with name', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const big = makeFile('big.txt', TEXT_MAX_BYTES + 1);
    Object.defineProperty(input, 'files', { value: [big], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText(/big\.txt/)).toBeTruthy();
    expect(onUpload).not.toHaveBeenCalled();
  });

  it('rejects oversized binary file and shows per-file error', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const bigPdf = makeFile('big.pdf', BINARY_MAX_BYTES + 1, 'application/pdf');
    Object.defineProperty(input, 'files', { value: [bigPdf], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText(/big\.pdf/)).toBeTruthy();
    expect(onUpload).not.toHaveBeenCalled();
  });

  it('rejects unsupported extension and shows per-file error', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const bad = makeFile('bad.exe', 100);
    Object.defineProperty(input, 'files', { value: [bad], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText(/bad\.exe/)).toBeTruthy();
    expect(onUpload).not.toHaveBeenCalled();
  });

  it('calls onUpload for valid md file via input change', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const ok = makeFile('ok.md', 100);
    Object.defineProperty(input, 'files', { value: [ok], writable: false });
    fireEvent.change(input);
    expect(onUpload).toHaveBeenCalledTimes(1);
    expect(onUpload).toHaveBeenCalledWith(expect.objectContaining({ name: 'ok.md' }));
  });

  it('calls onUpload for each valid file but shows per-file invalid for bad ones', async () => {
    const onUpload = vi.fn().mockResolvedValue(undefined);
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const ok = makeFile('ok.pdf', 100, 'application/pdf');
    const bad = makeFile('bad.exe', 100);
    Object.defineProperty(input, 'files', { value: [ok, bad], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText(/bad\.exe/)).toBeTruthy();
    expect(onUpload).toHaveBeenCalledTimes(1);
    expect(onUpload).toHaveBeenCalledWith(expect.objectContaining({ name: 'ok.pdf' }));
  });

  it('handles drag over / leave / drop and triggers onUpload on drop', async () => {
    const onUpload = vi.fn().mockResolvedValue(undefined);
    render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const zone = screen.getByRole('button');

    fireEvent.dragOver(zone);
    expect(zone.className).toContain('border-primary');

    fireEvent.dragLeave(zone);
    expect(zone.className).not.toContain('border-primary');

    const file = makeFile('drag.md', 100);
    fireEvent.dragOver(zone);
    fireEvent.drop(zone, { dataTransfer: { files: [file] } });
    await waitFor(() => expect(onUpload).toHaveBeenCalledTimes(1));
    expect(zone.className).not.toContain('border-primary');
  });

  it('shows formatted upload error when onUpload promise rejects (pending failed)', async () => {
    const onUpload = vi.fn(() => Promise.reject(new Error('boom')));
    const formatUploadError = vi.fn((err: unknown) => `upload failed: ${(err as Error).message}`);
    render(<DocUploadZone labels={labels} onUpload={onUpload} formatUploadError={formatUploadError} />);
    const zone = screen.getByRole('button');
    const file = makeFile('ok.md', 100);
    fireEvent.drop(zone, { dataTransfer: { files: [file] } });
    expect(await screen.findByText(/upload failed: boom/)).toBeTruthy();
    expect(formatUploadError).toHaveBeenCalled();
  });

  it('keyboard Enter triggers file picker', async () => {
    const { container } = render(<DocUploadZone labels={labels} onUpload={vi.fn()} />);
    const zone = screen.getByRole('button');
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const clickSpy = vi.spyOn(input, 'click');
    zone.focus();
    fireEvent.keyDown(zone, { key: 'Enter' });
    expect(clickSpy).toHaveBeenCalled();
  });

  it('boundary: text file at exactly TEXT_MAX_BYTES is accepted', async () => {
    const onUpload = vi.fn().mockResolvedValue(undefined);
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const exact = makeFile('exact.txt', TEXT_MAX_BYTES);
    Object.defineProperty(input, 'files', { value: [exact], writable: false });
    fireEvent.change(input);
    await waitFor(() => expect(onUpload).toHaveBeenCalledTimes(1));
  });

  it('renders pending uploading and failed states', async () => {
    const onUpload = vi.fn(() => new Promise(() => {}));
    render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const zone = screen.getByRole('button');
    const file = makeFile('pending.md', 100);
    fireEvent.drop(zone, { dataTransfer: { files: [file] } });
    expect(await screen.findByText('pending.md')).toBeTruthy();
    expect(screen.getByText('ks.uploading')).toBeTruthy();
  });
});
