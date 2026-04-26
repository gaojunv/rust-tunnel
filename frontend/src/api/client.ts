import axios from 'axios';
import type {
  ClientResponse,
  PortTraffic,
  ServerMetrics,
  LoginRequest,
  LoginResponse,
} from '../types';

const API_BASE = '/api';

// Create axios instance
const api = axios.create({
  baseURL: API_BASE,
});

// Add auth token to requests
api.interceptors.request.use((config) => {
  const token = localStorage.getItem('auth_token');
  if (token) {
    config.headers.Authorization = `Bearer ${token}`;
  }
  return config;
});

// Handle 401 responses
api.interceptors.response.use(
  (response) => response,
  (error) => {
    if (error.response?.status === 401) {
      localStorage.removeItem('auth_token');
      window.location.href = '/login';
    }
    return Promise.reject(error);
  }
);

// Auth API
export const login = async (data: LoginRequest): Promise<LoginResponse> => {
  const response = await api.post<LoginResponse>('/login', data);
  if (response.data.token) {
    localStorage.setItem('auth_token', response.data.token);
  }
  return response.data;
};

export const logout = async (): Promise<void> => {
  await api.post('/logout');
  localStorage.removeItem('auth_token');
};

// Clients API
export const getClients = async (): Promise<ClientResponse[]> => {
  const response = await api.get<ClientResponse[]>('/clients');
  return response.data;
};

export const disconnectClient = async (port: number): Promise<void> => {
  await api.delete(`/clients/${port}`);
};

// Traffic API
export const getTraffic = async (): Promise<PortTraffic[]> => {
  const response = await api.get<PortTraffic[]>('/traffic');
  return response.data;
};

export const getPortTraffic = async (port: number): Promise<PortTraffic> => {
  const response = await api.get<PortTraffic>(`/traffic/${port}`);
  return response.data;
};

// Metrics API
export const getMetrics = async (): Promise<ServerMetrics> => {
  const response = await api.get<ServerMetrics>('/metrics');
  return response.data;
};

// Health check
export const checkHealth = async (): Promise<{ status: string }> => {
  const response = await api.get('/health');
  return response.data;
};

// Check if we have a token
export const isAuthenticated = (): boolean => {
  return !!localStorage.getItem('auth_token');
};
