import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';

interface QualityBadgeProps {
  score: number;
  className?: string;
}

function getQualityLabel(score: number): string {
  if (score >= 90) return 'Excellent';
  if (score >= 70) return 'Good';
  if (score >= 50) return 'Fair';
  return 'Poor';
}

function getQualityVariant(score: number): 'default' | 'secondary' | 'destructive' | 'outline' {
  if (score >= 90) return 'default';
  if (score >= 70) return 'secondary';
  if (score >= 50) return 'outline';
  return 'destructive';
}

function getQualityClassName(score: number): string {
  if (score >= 90) return 'bg-green-100 text-green-800 dark:bg-green-900 dark:text-green-100 border-green-200 dark:border-green-800';
  if (score >= 70) return 'bg-yellow-100 text-yellow-800 dark:bg-yellow-900 dark:text-yellow-100 border-yellow-200 dark:border-yellow-800';
  if (score >= 50) return 'bg-orange-100 text-orange-800 dark:bg-orange-900 dark:text-orange-100 border-orange-200 dark:border-orange-800';
  return 'bg-red-100 text-red-800 dark:bg-red-900 dark:text-red-100 border-red-200 dark:border-red-800';
}

export function QualityBadge({ score, className }: QualityBadgeProps) {
  const label = getQualityLabel(score);
  const variant = getQualityVariant(score);
  const qualityClassName = getQualityClassName(score);

  return (
    <Badge
      variant={variant}
      className={cn(qualityClassName, className)}
    >
      {label} ({score})
    </Badge>
  );
}
