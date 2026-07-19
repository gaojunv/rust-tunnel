interface ChartEmptyProps {
  message?: string;
  loading?: boolean;
}

export const ChartEmpty = ({ message = 'No data available', loading = false }: ChartEmptyProps) => (
  <div className="flex h-[200px] w-full items-center justify-center text-sm text-muted-foreground">
    {loading ? 'Loading...' : message}
  </div>
);
