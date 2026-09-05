export type SelectionSource = {
  getSelection(): { text: string; start: number; end: number } | null;
  getCaretRect(pos: number): { top: number; left: number } | null;
  replaceRange(from: number, to: number, text: string): void;
  insertAt(pos: number, text: string): void;
  focus(): void;
};
