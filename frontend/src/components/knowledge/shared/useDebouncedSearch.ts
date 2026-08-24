import { useEffect, useState } from 'react';

/** 受控防抖输入：本地输入即时响应，300ms 后提交到外层 filters。 */
export function useDebouncedSearch(
  value: string,
  onCommit: (v: string) => void,
  delay = 300,
) {
  const [input, setInput] = useState(value);
  useEffect(() => {
    setInput(value);
  }, [value]);
  useEffect(() => {
    const timer = setTimeout(() => {
      if (input !== value) onCommit(input);
    }, delay);
    return () => clearTimeout(timer);
  }, [input, value, onCommit, delay]);
  return [input, setInput] as const;
}
