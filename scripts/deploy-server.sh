#!/usr/bin/env bash
# deploy-server.sh — 本地构建并部署 rust-tunnel 服务端
# 蓝本: .github/workflows/release-server.yml  (单 job: 前端嵌入 → musl 静态编译 → 渲染配置 → SCP → SSH 重启)
# 用法: ./scripts/deploy-server.sh [--dry-run] [--skip-frontend] [--skip-build] [--target <triple>] [--host <host>] [--port <port>] [--user <user>] [--deploy-path <path>]
# 部署目标通过环境变量 / .env / --host 配置，不在脚本中硬编码服务器地址。
set -euo pipefail

# ---------- 颜色 ----------
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${CYAN}[INFO]${NC} $*"; }
ok()    { echo -e "${GREEN}[ OK ]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
die()   { echo -e "${RED}[FAIL]${NC} $*" >&2; exit 1; }

# ---------- 默认值 ----------
TARGET="x86_64-unknown-linux-musl"
SERVER_HOST="${SERVER_HOST:-}"
SERVER_PORT="${SERVER_PORT:-22}"
SERVER_USER="${SERVER_USER:-root}"
SERVER_SSH_KEY="${SERVER_SSH_KEY:-}"
DEPLOY_PATH="${DEPLOY_PATH:-}"
ADMIN_PASSWORD="${ADMIN_PASSWORD:-}"
CLIENT_AUTH_TOKEN="${CLIENT_AUTH_TOKEN:-}"
SS_PASSWORD="${SS_PASSWORD:-}"
DRY_RUN="${DEPLOY_DRY_RUN:-0}"
SKIP_FRONTEND=0
SKIP_BUILD=0

# ---------- 参数解析 ----------
usage() {
  cat <<EOF
用法: $(basename "$0") [选项]

选项:
  --dry-run              仅本地构建，不 SCP/SSH 推送
  --skip-frontend        跳过前端构建（复用现有 frontend-dist/）
  --skip-build           跳过 cargo build（复用已有二进制）
  --target <triple>      musl target，默认 x86_64-unknown-linux-musl
  --host <host>          覆盖 SERVER_HOST (必填，无默认值)
  --port <port>          覆盖 SERVER_PORT (默认 22)
  --user <user>          覆盖 SERVER_USER (默认 root)
  --deploy-path <path>   覆盖 DEPLOY_PATH (如 /opt/rust-tunnel)
  -h, --help             显示帮助

环境变量 (优先级: 命令行 > 环境变量 > .env 文件):
  SERVER_HOST (必填), SERVER_PORT, SERVER_USER, SERVER_SSH_KEY
  DEPLOY_PATH, ADMIN_PASSWORD, CLIENT_AUTH_TOKEN, SS_PASSWORD
  DEPLOY_DRY_RUN=1  等价于 --dry-run

示例:
  ./scripts/deploy-server.sh                        # 完整构建+部署（需先配置 SERVER_HOST）
  ./scripts/deploy-server.sh --dry-run              # 仅本地构建校验
  ./scripts/deploy-server.sh --skip-frontend        # 仅改后端时提速
  SERVER_HOST=your.example.com ./scripts/deploy-server.sh
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --skip-frontend) SKIP_FRONTEND=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --target) TARGET="$2"; shift 2 ;;
    --host) SERVER_HOST="$2"; shift 2 ;;
    --port) SERVER_PORT="$2"; shift 2 ;;
    --user) SERVER_USER="$2"; shift 2 ;;
    --deploy-path) DEPLOY_PATH="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "未知参数: $1  (用 --help 查看用法)" ;;
  esac
done

# ---------- 项目根目录 ----------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

# ---------- 加载 .env ----------
if [[ -f "${PROJECT_ROOT}/.env" ]]; then
  info "加载 .env ..."
  set -a
  # shellcheck disable=SC1091
  source "${PROJECT_ROOT}/.env"
  set +a
  # 命令行/预设环境变量优先：若解析前已有值则恢复（避免 .env 覆盖已导出的变量）
  # 做法：已在解析前用 :- 取默认，.env source 后若对应变量为空才需关注；此处简单提示
fi

# 重新应用命令行覆盖（若 .env 覆盖了它们）
# 实际上 source 会覆盖已有的空默认值，但不会覆盖调用者显式 export 的值（因 :- 已在顶部求值）
# 为确保命令行 --host 等生效，需在解析后再次以局部变量为准；已在 case 中直接赋值，无需回退

# DRY_RUN 可能是字符串 "1"/"true"
if [[ "${DRY_RUN}" == "1" || "${DRY_RUN}" == "true" ]]; then
  DRY_RUN=1
else
  DRY_RUN=0
fi

if [[ -z "${SERVER_HOST}" ]]; then
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    warn "SERVER_HOST 未设置（dry-run 下可忽略，完整部署时必填）"
  else
    die "SERVER_HOST 未设置，请通过 --host / SERVER_HOST 环境变量 / .env 配置（例如 SERVER_HOST=your.example.com）"
  fi
fi

info "配置: host=${SERVER_HOST}:${SERVER_PORT} user=${SERVER_USER} target=${TARGET} dry_run=${DRY_RUN} skip_frontend=${SKIP_FRONTEND} skip_build=${SKIP_BUILD}"
info "远端目录: ${DEPLOY_PATH:-<未设置，部署时必填>}"

# ---------- 前置检查 ----------
need_cmd() { command -v "$1" &>/dev/null || die "缺少命令: $1"; }
need_cmd cargo
need_cmd scp
need_cmd ssh
need_cmd sed

if [[ "${SKIP_FRONTEND}" -eq 0 ]]; then
  need_cmd npm
  if ! command -v node &>/dev/null; then
    die "缺少 node (需 Node 20，建议 nvm use 20)"
  fi
fi

# musl target 检查（仅当需要构建时）
if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  if ! rustup target list --installed 2>/dev/null | grep -q "${TARGET}"; then
    warn "未安装 target ${TARGET}，尝试安装..."
    rustup target add "${TARGET}" || die "安装 ${TARGET} 失败，请手动执行: rustup target add ${TARGET}"
  fi
  if ! command -v musl-gcc &>/dev/null; then
    warn "未找到 musl-gcc，静态链接可能失败。Debian/Ubuntu: sudo apt-get install -y musl-tools"
  fi
fi

# SSH 选项
SSH_OPTS=(-p "${SERVER_PORT}" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
SCP_OPTS=(-P "${SERVER_PORT}" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
if [[ -n "${SERVER_SSH_KEY}" ]]; then
  [[ -f "${SERVER_SSH_KEY}" ]] || die "SERVER_SSH_KEY 指向的文件不存在: ${SERVER_SSH_KEY}"
  SSH_OPTS+=(-i "${SERVER_SSH_KEY}")
  SCP_OPTS+=(-i "${SERVER_SSH_KEY}")
fi

# ---------- 1. 前端构建并嵌入 ----------
if [[ "${SKIP_FRONTEND}" -eq 1 ]]; then
  warn "跳过前端构建，复用现有 frontend-dist/"
  [[ -d frontend-dist ]] || warn "frontend-dist 不存在，嵌入的前端将为空！"
else
  info "━━━━━━━━ 1/5 前端构建 ━━━━━━━━━"
  # nvm 兼容：若通过 nvm 安装的 node，尝试加载
  if [[ -s "${HOME}/.nvm/nvm.sh" ]]; then
    # shellcheck disable=SC1090
    source "${HOME}/.nvm/nvm.sh" 2>/dev/null || true
  fi
  [[ -d frontend ]] || die "未找到 frontend/ 目录"
  (
    cd frontend
    if [[ -f package-lock.json ]]; then
      npm ci
    else
      npm install
    fi
    npm run build
  )
  rm -rf frontend-dist
  cp -r frontend/dist frontend-dist
  ok "前端已嵌入到 frontend-dist/ ($(du -sh frontend-dist | cut -f1))"
fi

# ---------- 2. 后端 musl 静态编译 ----------
if [[ "${SKIP_BUILD}" -eq 1 ]]; then
  warn "跳过 cargo build，复用已有二进制"
  [[ -f ./rust-tunnel-server ]] || [[ -f "target/${TARGET}/release/rust-tunnel-server" ]] || warn "未找到已有二进制，部署将失败"
else
  info "━━━━━━━━ 2/5 后端编译 (${TARGET}) ━━━━━━━━"
  # rag 依赖 qdrant-edge 需 nightly (array_windows 等不稳定特性)，有 nightly 则优先用 nightly
  CARGO_CMD="cargo"
  if rustup toolchain list 2>/dev/null | grep -q nightly; then
    CARGO_CMD="cargo +nightly"
  fi
  $CARGO_CMD build --release -p rust-tunnel-server --target "${TARGET}" --features rag,embed-frontend
  # strip 减小体积（若可用）
  if command -v strip &>/dev/null; then
    strip "target/${TARGET}/release/rust-tunnel-server" 2>/dev/null || true
  fi
  cp "target/${TARGET}/release/rust-tunnel-server" ./rust-tunnel-server
  BIN_SIZE=$(du -h ./rust-tunnel-server | cut -f1)
  BIN_INFO=$(file ./rust-tunnel-server 2>/dev/null | head -1 || echo "unknown")
  ok "编译完成: ./rust-tunnel-server (${BIN_SIZE}) — ${BIN_INFO}"
  if ! echo "${BIN_INFO}" | grep -qi "statically linked"; then
    warn "二进制似乎非静态链接，请检查 musl 工具链"
  fi
fi

# ---------- 3. 渲染 config.toml ----------
info "━━━━━━━━ 3/5 渲染 config.toml ━━━━━━━━"
if [[ "${DRY_RUN}" -eq 1 ]]; then
  # dry-run 下若缺 secrets 则跳过渲染，仅提示
  if [[ -z "${DEPLOY_PATH}" || -z "${ADMIN_PASSWORD}" || -z "${CLIENT_AUTH_TOKEN}" || -z "${SS_PASSWORD}" ]]; then
    warn "dry-run 且部分 Secrets 缺失，跳过 config.toml 渲染（完整部署时必填: DEPLOY_PATH/ADMIN_PASSWORD/CLIENT_AUTH_TOKEN/SS_PASSWORD）"
  else
    info "dry-run 但 Secrets 齐全，仍渲染 config.toml 供本地校验"
  fi
fi

NEED_RENDER=1
if [[ -z "${DEPLOY_PATH}" || -z "${ADMIN_PASSWORD}" || -z "${CLIENT_AUTH_TOKEN}" || -z "${SS_PASSWORD}" ]]; then
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    NEED_RENDER=0
  else
    echo ""
    echo -e "${RED}缺少必填 Secrets:${NC}"
    [[ -z "${DEPLOY_PATH}" ]] && echo "  - DEPLOY_PATH (远端落盘目录，如 /opt/rust-tunnel)"
    [[ -z "${ADMIN_PASSWORD}" ]] && echo "  - ADMIN_PASSWORD"
    [[ -z "${CLIENT_AUTH_TOKEN}" ]] && echo "  - CLIENT_AUTH_TOKEN"
    [[ -z "${SS_PASSWORD}" ]] && echo "  - SS_PASSWORD"
    echo ""
    echo "请在 .env 或环境变量中设置，例如:"
    echo "  cat > .env <<'ENV'"
    echo "  DEPLOY_PATH=/opt/rust-tunnel"
    echo "  ADMIN_PASSWORD=***"
    echo "  CLIENT_AUTH_TOKEN=***"
    echo "  SS_PASSWORD=***"
    echo "  ENV"
    die "Secrets 不完整，终止"
  fi
fi

if [[ "${NEED_RENDER}" -eq 1 ]]; then
  [[ -f contrib/config.toml.template ]] || die "未找到 contrib/config.toml.template"
  sed \
    -e "s|\${ADMIN_PASSWORD}|${ADMIN_PASSWORD}|g" \
    -e "s|\${CLIENT_AUTH_TOKEN}|${CLIENT_AUTH_TOKEN}|g" \
    -e "s|\${SS_PASSWORD}|${SS_PASSWORD}|g" \
    -e "s|\${TROJAN_PASSWORD}|${SS_PASSWORD}|g" \
    -e "s|\${CLIENT_DIST_DIR}|${DEPLOY_PATH%/}/client|g" \
    -e "s|\${WIKI_DIST_DIR}|${DEPLOY_PATH%/}/wiki|g" \
    contrib/config.toml.template > config.toml

  if grep -nE '\$\{(ADMIN_PASSWORD|CLIENT_AUTH_TOKEN|SS_PASSWORD|TROJAN_PASSWORD|CLIENT_DIST_DIR|WIKI_DIST_DIR)\}' config.toml; then
    die "config.toml 仍包含未替换的占位符，请检查 Secrets"
  fi
  ok "config.toml 已生成"
  echo "  client_dist_dir = $(grep client_dist_dir config.toml || true)"
  echo "  wiki_dist_dir   = $(grep wiki_dist_dir config.toml || true)"
fi

if [[ "${DRY_RUN}" -eq 1 ]]; then
  echo ""
  ok "dry-run 完成，未推送到远端。产物:"
  ls -lh ./rust-tunnel-server 2>/dev/null || echo "  (无二进制)"
  ls -lh config.toml 2>/dev/null || echo "  (未生成 config.toml)"
  echo ""
  echo "如需完整部署: ./scripts/deploy-server.sh  (移除 --dry-run)"
  exit 0
fi

# ---------- 4. 停止远端服务并推送 ----------
info "━━━━━━━━ 4/5 停止远端服务并推送到 ${SERVER_USER}@${SERVER_HOST}:${DEPLOY_PATH} ━━━━━━━━"
# 先测试 SSH 连通性
if ! ssh "${SSH_OPTS[@]}" "${SERVER_USER}@${SERVER_HOST}" "echo ok" 2>&1 | grep -q ok; then
  die "SSH 连接失败: ${SERVER_USER}@${SERVER_HOST}:${SERVER_PORT}  (检查 SERVER_HOST/PORT/USER/SSH_KEY)"
fi
ok "SSH 连通正常"

# 先停止服务，释放二进制占用（否则 scp/覆盖会报 Text file busy）
info "停止远端服务以释放二进制占用..."
ssh "${SSH_OPTS[@]}" "${SERVER_USER}@${SERVER_HOST}" "sudo systemctl stop rust-tunnel-server 2>&1 || true; sleep 1; systemctl is-active rust-tunnel-server 2>&1 || echo '已停止 (inactive)'"

# 远端备份上一版
ssh "${SSH_OPTS[@]}" "${SERVER_USER}@${SERVER_HOST}" "mkdir -p '${DEPLOY_PATH}' && if [[ -f '${DEPLOY_PATH}/rust-tunnel-server' ]]; then cp '${DEPLOY_PATH}/rust-tunnel-server' '${DEPLOY_PATH}/rust-tunnel-server.prev' && echo '已备份上一版到 rust-tunnel-server.prev'; fi"

# 推送文件（服务已停，可安全覆盖）
scp "${SCP_OPTS[@]}" ./rust-tunnel-server ./contrib/rust-tunnel-server.service ./config.toml "${SERVER_USER}@${SERVER_HOST}:${DEPLOY_PATH}/"
ok "文件已推送"

# ---------- 5. 远端安装并重启 ----------
info "━━━━━━━━ 5/5 远端安装并重启服务 ━━━━━━━━"
ssh "${SSH_OPTS[@]}" "${SERVER_USER}@${SERVER_HOST}" bash -s -- "${DEPLOY_PATH}" <<'REMOTE_EOF'
set -euo pipefail
DEPLOY_PATH="$1"
echo "[remote] 安装配置到 /etc/rust-tunnel/config.toml ..."
sudo mkdir -p /etc/rust-tunnel
sudo cp "${DEPLOY_PATH}/config.toml" /etc/rust-tunnel/config.toml
sudo chmod 600 /etc/rust-tunnel/config.toml

echo "[remote] 安装 systemd service ..."
sudo cp "${DEPLOY_PATH}/contrib/rust-tunnel-server.service" /etc/systemd/system/rust-tunnel-server.service 2>/dev/null \
  || sudo cp "${DEPLOY_PATH}/rust-tunnel-server.service" /etc/systemd/system/rust-tunnel-server.service
sudo systemctl daemon-reload

echo "[remote] 启动服务 ..."
sudo systemctl start rust-tunnel-server
sleep 2

echo "[remote] 服务状态:"
sudo systemctl is-active rust-tunnel-server && echo "[remote] ● active" || echo "[remote] ● 启动失败，查看日志: journalctl -u rust-tunnel-server -n 100 --no-pager"
sudo systemctl status rust-tunnel-server --no-pager | head -30 || true

echo "[remote] 健康检查 (curl /api/health) ..."
# api_addr 在模板中为 0.0.0.0:8081
if curl -sf http://127.0.0.1:8081/api/health 2>&1 | head -5; then
  echo "[remote] 健康检查通过"
else
  echo "[remote] 健康检查失败，尝试 3000 端口..."
  curl -sf http://127.0.0.1:3000/api/health 2>&1 | head -5 || echo "[remote] 健康检查未通过，请手动检查: journalctl -u rust-tunnel-server -n 100 --no-pager"
fi
REMOTE_EOF

ok "部署完成: https://${SERVER_HOST}/ (或 http://${SERVER_HOST}:8081/api/health)"
echo ""
echo "回滚上一版 (如需):"
echo "  ssh -p ${SERVER_PORT} ${SERVER_USER}@${SERVER_HOST} 'cp ${DEPLOY_PATH}/rust-tunnel-server.prev ${DEPLOY_PATH}/rust-tunnel-server && sudo systemctl restart rust-tunnel-server'"

# 本地明文 config.toml 可选清理
if [[ -f config.toml ]]; then
  echo ""
  warn "本地 config.toml 含明文密码，建议部署后清理: shred -u config.toml  或  rm config.toml"
fi
