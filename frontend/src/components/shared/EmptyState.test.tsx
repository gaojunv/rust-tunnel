// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Search } from 'lucide-react';
import { EmptyState } from './EmptyState';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('EmptyState', () => {
  it('renders title with default icon', () => {
    const { container } = render(<EmptyState title="No items" />);
    expect(screen.getByText('No items')).toBeTruthy();
    // 默认图标：Inbox， muted 前景 + 半透明
    const icon = container.querySelector('svg');
    expect(icon?.getAttribute('class')).toContain('text-muted-foreground');
  });

  it('renders custom icon and description', () => {
    render(<EmptyState icon={Search} title="Nothing found" description="Try another keyword" />);
    expect(screen.getByText('Nothing found')).toBeTruthy();
    expect(screen.getByText('Try another keyword')).toBeTruthy();
  });

  it('renders action node below', () => {
    const onClick = vi.fn();
    render(<EmptyState title="Empty" action={<button onClick={onClick}>Create one</button>} />);
    fireEvent.click(screen.getByText('Create one'));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it('merges custom className', () => {
    const { container } = render(<EmptyState title="Empty" className="custom-cls" />);
    expect(container.firstElementChild?.className).toContain('custom-cls');
  });
});
