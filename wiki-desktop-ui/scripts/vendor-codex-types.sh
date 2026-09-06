#!/usr/bin/env bash
set -euo pipefail

# Codex app-server 协议官方 TypeScript 类型 vendor 脚本
# 单一事实来源：CODEX_VERSION 可通过环境变量覆盖
# 注意：此默认值必须与 .github/workflows/release-wiki-client.yml 顶部 env.CODEX_VERSION 保持同步
CODEX_VERSION="${CODEX_VERSION:-rust-v0.153.4}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TARGET_DIR="${ROOT_DIR}/src/lib/agent/types"
PREFIX="codex-rs/app-server-protocol/schema/typescript/"

echo "[vendor-codex-types] CODEX_VERSION=${CODEX_VERSION}"
echo "[vendor-codex-types] TARGET_DIR=${TARGET_DIR}"

# 幂等：先清空目标目录再写入
rm -rf "${TARGET_DIR}"
mkdir -p "${TARGET_DIR}"

# 列出该 tag 下 PREFIX 目录的全部 .ts 文件（保持相对结构）
echo "[vendor-codex-types] listing .ts files via gh api..."
TMP_LIST="$(mktemp)"
trap 'rm -f "${TMP_LIST}"' EXIT

gh api "repos/openai/codex/git/trees/${CODEX_VERSION}?recursive=1" > "${TMP_LIST}"

# 用 python 解析 JSON，提取相对路径
mapfile -t REL_PATHS < <(python3 -c "
import json
with open('${TMP_LIST}', 'r') as f:
    data = json.load(f)
prefix = '${PREFIX}'
out = []
for t in data.get('tree', []):
    p = t.get('path', '')
    if p.startswith(prefix) and p.endswith('.ts'):
        out.append(p[len(prefix):])
for p in sorted(out):
    print(p)
")

COUNT="${#REL_PATHS[@]}"
if [[ "${COUNT}" -eq 0 ]]; then
  echo "[vendor-codex-types] ERROR: no .ts files found under ${PREFIX} at ${CODEX_VERSION}" >&2
  exit 1
fi
echo "[vendor-codex-types] found ${COUNT} .ts files"

# 逐个下载 raw 内容，保持相对目录结构
for rel in "${REL_PATHS[@]}"; do
  src_path="${PREFIX}${rel}"
  dest_path="${TARGET_DIR}/${rel}"
  mkdir -p "$(dirname "${dest_path}")"
  url="https://raw.githubusercontent.com/openai/codex/${CODEX_VERSION}/${src_path}"
  echo "[vendor-codex-types] fetching ${rel}"
  if ! curl -fsSL "${url}" -o "${dest_path}"; then
    echo "[vendor-codex-types] curl failed for ${rel}, trying gh api..." >&2
    gh api "repos/openai/codex/contents/${src_path}?ref=${CODEX_VERSION}" --jq '.content' 2>/dev/null | python3 -c "import sys, base64; sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))" > "${dest_path}" || {
      echo "[vendor-codex-types] ERROR: failed to fetch ${rel}" >&2
      exit 1
    }
  fi
  if [[ ! -s "${dest_path}" ]]; then
    echo "[vendor-codex-types] ERROR: empty file ${rel}" >&2
    exit 1
  fi
done

# 在目录顶部生成 VERSION 文件记录 pin 的版本
echo "${CODEX_VERSION}" > "${TARGET_DIR}/VERSION"
echo "[vendor-codex-types] done: ${COUNT} files -> ${TARGET_DIR} (VERSION=${CODEX_VERSION})"
