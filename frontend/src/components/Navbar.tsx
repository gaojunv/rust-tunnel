import { useMutation } from 'react-query';
import { logout } from '../api/client';

interface NavbarProps {
  onLogout: () => void;
}

export const Navbar = ({ onLogout }: NavbarProps) => {
  const logoutMutation = useMutation(logout, {
    onSuccess: () => {
      onLogout();
    },
  });

  const handleLogout = () => {
    logoutMutation.mutate();
  };

  return (
    <nav className="bg-gray-800">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex items-center justify-between h-16">
          <div className="flex items-center">
            <div className="flex-shrink-0">
              <h1 className="text-white text-xl font-bold">Rust Tunnel</h1>
            </div>
          </div>
          <div className="ml-4 flex items-center md:ml-6">
            <button
              onClick={handleLogout}
              className="bg-gray-700 hover:bg-gray-600 text-white px-4 py-2 rounded-md text-sm font-medium"
            >
              Logout
            </button>
          </div>
        </div>
      </div>
    </nav>
  );
};
