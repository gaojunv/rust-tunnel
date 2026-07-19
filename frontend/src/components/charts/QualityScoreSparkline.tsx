import { useQuality } from '@/api/hooks';
import { Sparkline } from './Sparkline';

export const QualityScoreSparkline = ({ port }: { port: number }) => {
  const { data, isError } = useQuality(port);

  if (isError || !data?.history?.length) {
    return null;
  }

  return (
    <Sparkline
      values={data.history.map((s) => s.quality_score)}
      className="mt-2 h-8 w-full"
    />
  );
};
