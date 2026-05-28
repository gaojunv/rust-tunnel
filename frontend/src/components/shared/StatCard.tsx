import React from 'react';

interface StatCardProps {
  label: string;
  value: string;
  icon: React.ReactNode;
  color?: 'blue' | 'green' | 'purple' | 'orange' | 'yellow' | 'red';
  valueColor?: string;
}

const colorClasses: Record<string, { bg: string }> = {
  blue: { bg: 'bg-blue-500' },
  green: { bg: 'bg-green-500' },
  purple: { bg: 'bg-purple-500' },
  orange: { bg: 'bg-orange-500' },
  yellow: { bg: 'bg-yellow-500' },
  red: { bg: 'bg-red-500' },
};

export const StatCard = ({ label, value, icon, color = 'blue', valueColor }: StatCardProps) => {
  const c = colorClasses[color];
  return (
    <div className="bg-white overflow-hidden shadow rounded-lg p-4 sm:p-6">
      <div className="flex items-center">
        <div className={`flex-shrink-0 ${c.bg} rounded-md p-3`}>
          {icon}
        </div>
        <div className="ml-5 w-0 flex-1">
          <dl>
            <dt className="text-sm font-medium text-gray-500 truncate">{label}</dt>
            <dd className={`text-lg font-semibold ${valueColor || 'text-gray-900'}`}>{value}</dd>
          </dl>
        </div>
      </div>
    </div>
  );
};
