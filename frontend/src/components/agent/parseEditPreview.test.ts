import { describe, it, expect } from 'vitest';
import { parseEditPreview } from './parseEditPreview';

describe('parseEditPreview', () => {
  it('returns null for empty string', () => {
    expect(parseEditPreview('')).toBeNull();
  });

  it('returns null for single line (no markers)', () => {
    expect(parseEditPreview('shell command summary')).toBeNull();
  });

  it('returns null when first line is a marker (not a path)', () => {
    expect(parseEditPreview('--- old\nfoo\n+++ new\nbar')).toBeNull();
  });

  it('returns null for non-edit format (no --- old or --- new content)', () => {
    expect(parseEditPreview('src/main.rs\nsome random content')).toBeNull();
  });

  describe('patch_file format (single edit)', () => {
    it('parses a single old/new edit', () => {
      const input = 'src/main.rs\n--- old\nold line 1\nold line 2\n+++ new\nnew line 1\nnew line 2';
      const result = parseEditPreview(input);
      expect(result).toEqual([
        {
          path: 'src/main.rs',
          old_text: 'old line 1\nold line 2',
          new_text: 'new line 1\nnew line 2',
        },
      ]);
    });
  });

  describe('edit_file format (multiple edits)', () => {
    it('parses multiple --- old / +++ new blocks', () => {
      const input = [
        'src/lib.rs',
        '--- old',
        'fn foo() {}',
        '+++ new',
        'fn foo() { println!("hi"); }',
        '--- old',
        'fn bar() {}',
        '+++ new',
        'fn bar() { println!("bye"); }',
      ].join('\n');
      const result = parseEditPreview(input);
      expect(result).toHaveLength(2);
      expect(result![0]).toEqual({
        path: 'src/lib.rs',
        old_text: 'fn foo() {}',
        new_text: 'fn foo() { println!("hi"); }',
      });
      expect(result![1]).toEqual({
        path: 'src/lib.rs',
        old_text: 'fn bar() {}',
        new_text: 'fn bar() { println!("bye"); }',
      });
    });

    it('handles trailing "... (N more edits)" line', () => {
      const input = [
        'src/lib.rs',
        '--- old',
        'aaa',
        '+++ new',
        'bbb',
        '... (3 more edits)',
      ].join('\n');
      const result = parseEditPreview(input);
      expect(result).toHaveLength(1);
      expect(result![0].old_text).toBe('aaa');
      expect(result![0].new_text).toBe('bbb');
    });
  });

  describe('write_file format', () => {
    it('parses --- new content marker', () => {
      const input = 'src/config.ts\n--- new content (2 lines shown / 10 total)\nline 1\nline 2';
      const result = parseEditPreview(input);
      expect(result).toEqual([
        {
          path: 'src/config.ts',
          old_text: null,
          new_text: 'line 1\nline 2',
        },
      ]);
    });

    it('strips truncation marker [... truncated]', () => {
      const input = 'src/config.ts\n--- new content (2 lines shown / 10 total)\nline 1\nline 2\n[... truncated, 10 total lines]';
      const result = parseEditPreview(input);
      expect(result).toEqual([
        {
          path: 'src/config.ts',
          old_text: null,
          new_text: 'line 1\nline 2',
        },
      ]);
    });
  });

  describe('edge cases', () => {
    it('handles empty old/new text', () => {
      const input = 'src/empty.ts\n--- old\n+++ new\n';
      const result = parseEditPreview(input);
      expect(result).toEqual([
        {
          path: 'src/empty.ts',
          old_text: null,
          new_text: null,
        },
      ]);
    });

    it('handles path with spaces (rare but possible)', () => {
      const input = 'my folder/file.ts\n--- old\nold\n+++ new\nnew';
      const result = parseEditPreview(input);
      expect(result).toEqual([
        {
          path: 'my folder/file.ts',
          old_text: 'old',
          new_text: 'new',
        },
      ]);
    });
  });
});
