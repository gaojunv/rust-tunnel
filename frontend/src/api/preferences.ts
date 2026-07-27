import { api } from './client';

export interface Preferences {
  theme: string;
  language: string;
  title_effect: string;
}

export async function fetchPreferences(): Promise<Preferences> {
  const response = await api.get<Preferences>('/preferences');
  return response.data;
}

export async function updatePreferences(prefs: Preferences): Promise<void> {
  await api.put('/preferences', prefs);
}
