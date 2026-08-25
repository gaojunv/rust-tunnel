// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import DocUploadZone, { TEXT_MAX_BYTES, BINARY_MAX_BYTES } from './DocUploadZone';

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
    // Loader2 has animate-spin, FileUp does not (when not uploading)
    expect(container.querySelector('.animate-spin')).toBeTruthy();
  });

  it('rejects oversized text file and shows fileInvalid, does not call onUpload', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const big = makeFile('big.txt', TEXT_MAX_BYTES + 1);
    // jsdom file input: set files via Object.defineProperty then fire change
    Object.defineProperty(input, 'files', { value: [big], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText('invalid file')).toBeTruthy();
    expect(onUpload).not.toHaveBeenCalled();
  });

  it('rejects oversized binary file and shows fileInvalid', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const bigPdf = makeFile('big.pdf', BINARY_MAX_BYTES + 1, 'application/pdf');
    Object.defineProperty(input, 'files', { value: [bigPdf], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText('invalid file')).toBeTruthy();
    expect(onUpload).not.toHaveBeenCalled();
  });

  it('rejects unsupported extension and shows fileInvalid', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const bad = makeFile('bad.exe', 100);
    Object.defineProperty(input, 'files', { value: [bad], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText('invalid file')).toBeTruthy();
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

  it('calls onUpload for each valid file but still shows invalid when mixed', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const ok = makeFile('ok.pdf', 100, 'application/pdf');
    const bad = makeFile('bad.exe', 100);
    Object.defineProperty(input, 'files', { value: [ok, bad], writable: false });
    fireEvent.change(input);
    expect(await screen.findByText('invalid file')).toBeTruthy();
    expect(onUpload).toHaveBeenCalledTimes(1);
    expect(onUpload).toHaveBeenCalledWith(expect.objectContaining({ name: 'ok.pdf' }));
  });

  it('handles drag over / leave / drop and triggers onUpload on drop', async () => {
    const onUpload = vi.fn();
    render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const zone = screen.getByRole('button');

    fireEvent.dragOver(zone);
    // dragging state toggles border-primary class
    expect(zone.className).toContain('border-primary');

    fireEvent.dragLeave(zone);
    expect(zone.className).not.toContain('border-primary');

    const file = makeFile('drag.md', 100);
    fireEvent.dragOver(zone);
    fireEvent.drop(zone, { dataTransfer: { files: [file] } });
    expect(onUpload).toHaveBeenCalledTimes(1);
    // drop should clear dragging
    expect(zone.className).not.toContain('border-primary');
  });

  it('shows formatted upload error when onUpload promise rejects', async () => {
    const onUpload = vi.fn(() => Promise.reject(new Error('boom')));
    const formatUploadError = vi.fn((err: unknown) => `upload failed: ${(err as Error).message}`);
    render(<DocUploadZone labels={labels} onUpload={onUpload} formatUploadError={formatUploadError} />);
    const zone = screen.getByRole('button');
    const file = makeFile('ok.md', 100);
    fireEvent.drop(zone, { dataTransfer: { files: [file] } });
    expect(await screen.findByText('upload failed: boom')).toBeTruthy();
    expect(formatUploadError).toHaveBeenCalled();
  });

  it('boundary: text file at exactly TEXT_MAX_BYTES is accepted', async () => {
    const onUpload = vi.fn();
    const { container } = render(<DocUploadZone labels={labels} onUpload={onUpload} />);
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    const exact = makeFile('exact.txt', TEXT_MAX_BYTES);
    Object.defineProperty(input, 'files', { value: [exact], writable: false });
    fireEvent.change(input);
    await waitFor(() => expect(onUpload).toHaveBeenCalledTimes(1));
    expect(screen.queryByText('invalid file')).toBeNull();
  });
});
