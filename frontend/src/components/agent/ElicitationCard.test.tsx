// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import ElicitationCard from './ElicitationCard';
import type { ChatItem } from './types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(cleanup);

const baseItem: ChatItem = {
  kind: 'elicitation',
  content: '',
  elicitationId: 'req1',
  elicitationMessage: 'Choose a color',
  elicitationStatus: 'pending',
};

describe('ElicitationCard', () => {
  it('renders message and single-select options, gates submit on required', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1: {
            type: 'string',
            title: 'Favorite color',
            description: 'Pick one',
            oneOf: [
              { const: 'red', title: 'Red', _meta: { _claude: { askUserQuestionOption: { preview: '#ff0000' } } } },
              { const: 'blue', title: 'Blue' },
            ],
          },
        },
        required: ['question_1'],
      },
    };
    render(<ElicitationCard item={item} onRespond={onRespond} />);
    // 消息与卡片标题渲染
    expect(screen.getByText('Choose a color')).toBeTruthy();
    expect(screen.getByText(/agent\.elicitationRequired/)).toBeTruthy();
    // 单选选项：title + description + 选项 preview（_meta._claude.askUserQuestionOption.preview）
    expect(screen.getByText('Pick one')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Red/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: /Blue/ })).toBeTruthy();
    expect(screen.getByText('#ff0000')).toBeTruthy();
    // 必填未选 → 提交禁用
    const submit = screen.getByRole('button', { name: 'agent.elicitationSubmit' }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    // 选中 Red → 提交可用；点击提交 → accept + content
    fireEvent.click(screen.getByRole('button', { name: /Red/ }));
    expect((screen.getByRole('button', { name: 'agent.elicitationSubmit' }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationSubmit' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'accept', { question_1: 'red' });
  });

  it('toggles multi-select options and submits the selected array', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      elicitationMessage: 'Pick languages',
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1: {
            type: 'array',
            title: 'Languages',
            items: {
              anyOf: [
                { const: 'ts', title: 'TypeScript' },
                { const: 'rs', title: 'Rust' },
              ],
            },
          },
        },
      },
    };
    render(<ElicitationCard item={item} onRespond={onRespond} />);
    const tsBtn = screen.getByRole('button', { name: 'TypeScript' });
    const rsBtn = screen.getByRole('button', { name: 'Rust' });
    // 选中两项 → 再点 TypeScript 取消
    fireEvent.click(tsBtn);
    fireEvent.click(rsBtn);
    expect(tsBtn.getAttribute('aria-pressed')).toBe('true');
    expect(rsBtn.getAttribute('aria-pressed')).toBe('true');
    fireEvent.click(tsBtn);
    expect(tsBtn.getAttribute('aria-pressed')).toBe('false');
    // 提交 → 只带仍选中的 Rust
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationSubmit' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'accept', { question_1: ['rs'] });
  });

  it('renders the AskUserQuestion "Other" free-text input', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      elicitationMessage: 'Choose',
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1_custom: {
            type: 'string',
            title: 'Other',
            _meta: { _askUserQuestionCustomAnswer: true },
          },
        },
      },
    };
    render(<ElicitationCard item={item} onRespond={onRespond} />);
    const input = screen.getByLabelText('Other') as HTMLInputElement;
    expect(input).toBeTruthy();
    fireEvent.change(input, { target: { value: 'my custom answer' } });
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationSubmit' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'accept', { question_1_custom: 'my custom answer' });
  });

  it('renders boolean Switch and number Input with parsed value', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      elicitationMessage: 'Configure',
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1: { type: 'boolean', title: 'Verbose' },
          question_2: { type: 'integer', title: 'Retries', minimum: 1, maximum: 5 },
        },
      },
    };
    render(<ElicitationCard item={item} onRespond={onRespond} />);
    const sw = screen.getByLabelText('Verbose') as HTMLButtonElement;
    expect(sw.getAttribute('aria-checked')).toBe('false');
    fireEvent.click(sw);
    expect(screen.getByLabelText('Verbose').getAttribute('aria-checked')).toBe('true');
    const num = screen.getByLabelText('Retries') as HTMLInputElement;
    fireEvent.change(num, { target: { value: '3' } });
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationSubmit' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'accept', { question_1: true, question_2: 3 });
  });

  it('declines and cancels via footer buttons even when required unfilled', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1: { type: 'string', title: 'Name' },
        },
        required: ['question_1'],
      },
    };
    render(<ElicitationCard item={item} onRespond={onRespond} />);
    // 必填未填时提交禁用，但跳过/取消仍可用
    expect((screen.getByRole('button', { name: 'agent.elicitationSubmit' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationDecline' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'decline');
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationCancel' }));
    expect(onRespond).toHaveBeenCalledWith('req1', 'cancel');
  });

  it('renders terminal badge and hides form controls when not pending', () => {
    const item: ChatItem = {
      ...baseItem,
      elicitationStatus: 'accepted',
      elicitationSchema: {
        type: 'object',
        properties: {
          question_1: { type: 'string', title: 'Name' },
        },
      },
    };
    render(<ElicitationCard item={item} onRespond={vi.fn()} />);
    // 终态徽章：已提交
    expect(screen.getByText(/agent\.elicitationAnswered/)).toBeTruthy();
    // 操作按钮与表单控件全部消失
    expect(screen.queryByRole('button', { name: 'agent.elicitationSubmit' })).toBeNull();
    expect(screen.queryByLabelText('Name')).toBeNull();
  });

  it('shows declined and cancelled terminal badges', () => {
    const mk = (status: ChatItem['elicitationStatus']) => ({
      ...baseItem,
      elicitationStatus: status,
      elicitationSchema: { type: 'object' as const, properties: {} },
    });
    const { unmount } = render(<ElicitationCard item={mk('declined')} onRespond={vi.fn()} />);
    expect(screen.getByText(/agent\.elicitationDeclined/)).toBeTruthy();
    unmount();
    render(<ElicitationCard item={mk('cancelled')} onRespond={vi.fn()} />);
    expect(screen.getByText('agent.elicitationCancelled')).toBeTruthy();
  });
});
