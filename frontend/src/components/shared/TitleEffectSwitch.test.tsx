// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as apiModule from '../../api/preferences';
import { PreferencesProvider } from '../../preferences/PreferencesProvider';
import { PREFERENCES_CACHE_KEY } from '../../preferences/preferencesStore';
import { TitleEffectSwitch } from './TitleEffectSwitch';

vi.mock('../../api/preferences', () => ({
  fetchPreferences: vi.fn(),
  updatePreferences: vi.fn().mockResolvedValue(undefined),
}));

vi.mock('./ParticleTitle', () => ({
  ParticleTitle: ({ text }: { text: string }) => <div data-testid="particle-title">{text}</div>,
}));

vi.mock('./GridWaveTitle', () => ({
  GridWaveTitle: ({ text }: { text: string }) => <div data-testid="grid-wave-title">{text}</div>,
}));

function renderWithPrefs(cacheValue: string | null) {
  if (cacheValue !== null) {
    localStorage.setItem(PREFERENCES_CACHE_KEY, cacheValue);
  }
  return render(
    <PreferencesProvider>
      <TitleEffectSwitch text="Hello" />
    </PreferencesProvider>,
  );
}

describe('TitleEffectSwitch', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    vi.mocked(apiModule.fetchPreferences).mockResolvedValue({
      theme: 'dark',
      language: 'system',
      title_effect: 'grid-wave',
    });
  });

  afterEach(() => {
    cleanup();
  });

  it('renders GridWaveTitle by default (grid-wave)', async () => {
    renderWithPrefs(null);
    await waitFor(() => {
      expect(screen.getByTestId('grid-wave-title')).toBeTruthy();
    });
  });

  it('renders ParticleTitle when mode = particles', async () => {
    vi.mocked(apiModule.fetchPreferences).mockResolvedValue({
      theme: 'dark',
      language: 'system',
      title_effect: 'particles',
    });
    renderWithPrefs(null);
    await waitFor(() => {
      expect(screen.getByTestId('particle-title')).toBeTruthy();
    });
  });

  it('renders plain span when mode = none', async () => {
    vi.mocked(apiModule.fetchPreferences).mockResolvedValue({
      theme: 'dark',
      language: 'system',
      title_effect: 'none',
    });
    const { container } = renderWithPrefs(null);
    await waitFor(() => {
      expect(container.querySelector('[data-testid="grid-wave-title"]')).toBeNull();
      expect(container.querySelector('[data-testid="particle-title"]')).toBeNull();
      expect(container.textContent).toContain('Hello');
    });
  });

  it('uses cached preference before server response', () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'dark', language: 'system', titleEffect: 'particles' }),
    );
    vi.mocked(apiModule.fetchPreferences).mockImplementation(() => new Promise(() => {}));
    render(
      <PreferencesProvider>
        <TitleEffectSwitch text="Hello" />
      </PreferencesProvider>,
    );
    expect(screen.getByTestId('particle-title')).toBeTruthy();
  });
});
