import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';

interface QualityBadgeProps {
  score: number;
  className?: string;
}

export function QualityBadge({ score, className }: QualityBadgeProps) {
  const getLabel = () => {
    if (score >= 80) return 'Excellent';
    if (score >= 60) return 'Good';
    if (score >= 40) return 'Fair';
    return 'Poor';
  };

  const getTone = () => {
    if (score >= 80) return 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25';
    if (score >= 60) return 'bg-sky-500/10 text-sky-500 border-sky-500/25';
    if (score >= 40) return 'bg-amber-500/10 text-amber-500 border-amber-500/25';
    return 'bg-red-500/10 text-red-500 border-red-500/25';
  };

  const getDot = () => {
    if (score >= 80) return 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]';
    if (score >= 60) return 'bg-sky-500 shadow-[0_0_6px_hsl(199_89%_55%/0.8)]';
    if (score >= 40) return 'bg-amber-500 shadow-[0_0_6px_hsl(38_92%_55%/0.8)]';
    return 'bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]';
  };

  return (
    <Badge variant="outline" className={cn('gap-1.5 font-medium', getTone(), className)}>
      <span className={cn('h-1.5 w-1.5 rounded-full', getDot())} />
      {getLabel()} ({score})
    </Badge>
  );
}
