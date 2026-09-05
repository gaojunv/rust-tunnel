import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';

export default tseslint.config(
  {
    ignores: ['dist/**', 'node_modules/**'],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['src/**/*.{ts,tsx}'],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
      globals: {
        console: 'readonly',
        document: 'readonly',
        fetch: 'readonly',
        localStorage: 'readonly',
        window: 'readonly',
        confirm: 'readonly',
        EventSource: 'readonly',
      },
    },
    plugins: {
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      'react-refresh/only-export-components': 'off',
      'no-undef': 'off',
      '@typescript-eslint/no-unused-vars': [
        'error',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_' },
      ],
      // react-compiler / react-hooks v7 的新增规则与本项目现有模式冲突：
      // - set-state-in-effect：项目多处“数据到达后回填表单/受控状态”是既有且正确的受控同步模式
      // - refs / purity / incompatible-library / immutability / globals：均为 v7 的 react-compiler 规则，
      //   本项目不启用 React Compiler，相关约束与现有代码（TanStack Virtual、xterm、即时 ref 同步等）不匹配
      'react-hooks/set-state-in-effect': 'off',
      'react-hooks/refs': 'off',
      'react-hooks/purity': 'off',
      'react-hooks/incompatible-library': 'off',
      'react-hooks/immutability': 'off',
      'react-hooks/globals': 'off',
    },
  },
  {
    files: ['src/test-setup.ts', 'src/**/*.test.ts', 'src/**/*.test.tsx'],
    rules: {
      'no-dupe-class-members': 'off',
    },
  },
  {
    files: ['vite.config.ts'],
    languageOptions: {
      globals: {
        __dirname: 'readonly',
        process: 'readonly',
      },
    },
  },
);
