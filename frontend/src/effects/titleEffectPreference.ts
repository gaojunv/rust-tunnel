export type TitleEffectPreference = 'particles' | 'grid-wave' | 'none';

export const TITLE_EFFECT_PREFERENCES: readonly TitleEffectPreference[] = [
  'particles',
  'grid-wave',
  'none',
] as const;

export const DEFAULT_TITLE_EFFECT: TitleEffectPreference = 'grid-wave';

export function isTitleEffectPreference(value: unknown): value is TitleEffectPreference {
  return (
    typeof value === 'string' &&
    (TITLE_EFFECT_PREFERENCES as readonly string[]).includes(value)
  );
}
