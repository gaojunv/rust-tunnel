import { useTranslation } from 'react-i18next';
import { getApiErrorMessage } from '../../../../api/client';
import { githubErrorKind, githubErrorTitleKey } from './githubUtils';

/** GitHub 面板列表错误横幅：按错误类别给标题（限流/无效 token/上游错误），
 *  正文透出后端 message（已含具体原因，如 401 详情 / 上游 404 消息）。 */
export function GithubErrorBanner({ error }: { error: unknown }) {
  const { t } = useTranslation();
  // 标题 key 由错误类别动态拼接，需宽签名 t（i18next 严格 key 联合不适用）
  const translate = t as (key: string) => string;
  const kind = githubErrorKind(error);
  return (
    <div className="space-y-0.5 px-1" role="alert" data-testid="github-error-banner">
      <p className="text-xs font-medium text-destructive">{translate(githubErrorTitleKey(kind))}</p>
      <p className="text-xs text-muted-foreground">{getApiErrorMessage(error)}</p>
    </div>
  );
}

/** 写操作（rerun/cancel/dispatch）错误提示：单行红色文案。 */
export function GithubMutationError({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <p className="px-1 text-xs text-destructive" role="alert" data-testid="github-mutation-error">
      {error}
    </p>
  );
}
