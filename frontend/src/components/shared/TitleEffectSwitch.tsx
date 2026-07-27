import { usePreferences } from '../../preferences/PreferencesProvider';
import { cn } from '../../lib/utils';
import { GridWaveTitle } from './GridWaveTitle';
import { ParticleTitle } from './ParticleTitle';

export interface TitleEffectSwitchProps {
  text: string;
  className?: string;
  eventTargetRef?: React.RefObject<HTMLElement | null>;
}

export function TitleEffectSwitch({ text, className, eventTargetRef }: TitleEffectSwitchProps) {
  const { prefs } = usePreferences();

  switch (prefs.titleEffect) {
    case 'particles':
      return <ParticleTitle text={text} className={className} eventTargetRef={eventTargetRef} />;
    case 'grid-wave':
      return <GridWaveTitle text={text} className={className} eventTargetRef={eventTargetRef} />;
    case 'none':
      return <span className={cn('text-aurora', className)}>{text}</span>;
  }
}
