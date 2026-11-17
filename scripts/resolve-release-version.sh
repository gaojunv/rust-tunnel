#!/usr/bin/env bash
#
# 解析发布归档目录名（服务端 ${DEPLOY_PATH}/client/<dir> 与 ${DEPLOY_PATH}/wiki/<dir>）。
#
# 为什么要有它：版本目录名此前的来源各不相同 —— CLI 直接取 GITHUB_REF_NAME（手动触发
# 时那是**分支名**，产物于是落到 client/main 这种非版本目录里），GUI 要求手动触发时
# 手填 version，wiki 手动触发落到 dev-<sha7>。而真正的事实来源一直在仓库里：crate 清单
# 的 `[package] version` 就是客户端上报给服务端做版本门控的那个数字（CARGO_PKG_VERSION，
# 见 crates/agent/src/runner/version_gate.rs）——发布目录名就该由它推出来。
#
# 解析优先级：
#   1. --input <名字>    显式覆盖（workflow_dispatch 的 version 输入，一般不用填）
#   2. tag 触发          目录 = tag 去掉 --tag-prefix，且**必须**等于 v<清单版本>
#                        （tag 与清单版本不一致时拒绝发布，理由见下面的报错文本）
#   3. 其余（手动触发）  目录 = v<清单版本>，不需要任何人手记版本号
#
# 用法：
#   bash scripts/resolve-release-version.sh --manifest crates/client/Cargo.toml
#   bash scripts/resolve-release-version.sh --manifest crates/client-gui/Cargo.toml --tag-prefix gui-
#   bash scripts/resolve-release-version.sh --manifest crates/wiki-serve/Cargo.toml \
#        --tag-prefix wiki- --cross-check crates/wiki-serve/tauri.conf.json
#
# 输出（$GITHUB_OUTPUT 存在时写进去，否则打到 stdout，便于本地直接跑）：
#   dir=v0.17.0      归档目录名（带 v：下载页按 semver 解析版本目录）
#   version=0.17.0   清单里的裸版本号
#   source=manifest|input|tag
#
# 退出码非 0 = 清单读不出来 / 目录名非法 / tag 与清单版本不一致。

set -euo pipefail

manifest=""
tag_prefix=""
input_version=""
cross_check=""

while [ $# -gt 0 ]; do
  case "$1" in
    --manifest) manifest="${2:?--manifest 需要一个路径}"; shift 2 ;;
    --tag-prefix) tag_prefix="${2-}"; shift 2 ;;
    --input) input_version="${2-}"; shift 2 ;;
    --cross-check) cross_check="${2:?--cross-check 需要一个路径}"; shift 2 ;;
    -h | --help) sed -n '2,40p' "$0"; exit 0 ;;
    *)
      echo "::error::未知参数: $1" >&2
      exit 2
      ;;
  esac
done

if [ -z "${manifest}" ]; then
  echo "::error::缺少 --manifest <crate 的 Cargo.toml>" >&2
  exit 2
fi
if [ ! -f "${manifest}" ]; then
  echo "::error::找不到清单: ${manifest}" >&2
  exit 1
fi

# ── 读版本号 ────────────────────────────────────────────────────────
# 只认 `[package]` 段里的 version：`[dependencies.*]` 里也有 `version = "..."`，
# 全局 grep 会读错。`version.workspace = true` 回退到根清单的 [workspace.package]。
cargo_package_version() {
  awk '
    /^[[:space:]]*\[/ {
      in_pkg = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/)
      next
    }
    in_pkg && /^[[:space:]]*version[[:space:]]*\.[[:space:]]*workspace[[:space:]]*=/ {
      print "workspace"
      exit
    }
    in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*/, "", line)
      gsub(/["[:space:]]/, "", line)
      print line
      exit
    }
  ' "$1"
}

workspace_package_version() {
  awk '
    /^[[:space:]]*\[/ {
      in_pkg = ($0 ~ /^[[:space:]]*\[workspace\.package\][[:space:]]*$/)
      next
    }
    in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^=]*=[[:space:]]*/, "", line)
      gsub(/["[:space:]]/, "", line)
      print line
      exit
    }
  ' "$1"
}

# tauri.conf.json 的顶层 "version": "0.4.0"。Tauri 打包器以它为准（而不是 Cargo.toml），
# 所以 wiki 那边两者必须同时抬版，否则会在 v0.5.0 的目录里放 0.4.0 的安装包。
json_top_level_version() {
  sed -n 's/^[[:space:]]\{1,2\}"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}

toml_version="$(cargo_package_version "${manifest}")"
if [ "${toml_version}" = "workspace" ]; then
  if [ ! -f Cargo.toml ]; then
    echo "::error::${manifest} 用 version.workspace = true，但当前目录没有根 Cargo.toml" >&2
    exit 1
  fi
  toml_version="$(workspace_package_version Cargo.toml)"
fi
if [ -z "${toml_version}" ]; then
  echo "::error::读不出 ${manifest} 的 [package] version（清单格式变了？）" >&2
  exit 1
fi

if [ -n "${cross_check}" ]; then
  case "${cross_check}" in
    *.toml) other_version="$(cargo_package_version "${cross_check}")" ;;
    *.json) other_version="$(json_top_level_version "${cross_check}")" ;;
    *)
      echo "::error::--cross-check 只支持 .toml 与 .json: ${cross_check}" >&2
      exit 2
      ;;
  esac
  if [ "${other_version}" != "${toml_version}" ]; then
    echo "::error::版本号不一致: ${manifest} = ${toml_version}，${cross_check} = ${other_version:-<读不出>}" >&2
    echo "::error::这两处必须一起抬版（前者是二进制上报的版本，后者是安装包/目录版本）。" >&2
    exit 1
  fi
fi

# ── 定目录名 ────────────────────────────────────────────────────────
expected_dir="v${toml_version}"
ref_type="${GITHUB_REF_TYPE:-}"
ref_name="${GITHUB_REF_NAME:-}"

if [ -n "${input_version}" ]; then
  dir="${input_version}"
  source="input"
elif [ "${ref_type}" = "tag" ] && [[ "${ref_name}" == "${tag_prefix}"* ]]; then
  # gui-v0.18.6 + --tag-prefix gui-  ->  v0.18.6
  # wiki-v0.4.0 + --tag-prefix wiki- ->  v0.4.0
  dir="${ref_name#"${tag_prefix}"}"
  source="tag"
else
  # 手动触发（或手动触发时选的是不带本流程前缀的 tag）：直接取清单版本
  dir="${expected_dir}"
  source="manifest"
fi

# 与服务端 is_safe_segment 同一把尺子（只放行 A-Za-z0-9._+-）：非法目录名会被下载页
# 静默过滤，表现为「上传成功但页面看不见」，所以在这里就挡住。
if ! printf '%s' "${dir}" | grep -Eq '^[A-Za-z0-9._+-]{1,128}$' ||
  [ "${dir}" = "." ] || [ "${dir}" = ".." ]; then
  echo "::error::非法归档目录名: '${dir}'（允许字符：A-Za-z0-9._+-）" >&2
  exit 1
fi

if [ "${dir}" != "${expected_dir}" ]; then
  if [ "${source}" = "tag" ]; then
    # 这里必须硬失败，不能只告警：客户端上报给服务端的版本是 CARGO_PKG_VERSION，
    # 也就是清单里的这个数字。tag 与它不一致时，产物会被贴上一个它自己并不认识的
    # 版本号——服务端的版本门控（MIN_REMOTE_CLIENT_VERSION / MIN_UPDATE_CLIENT_VERSION）
    # 会按错的对象判断，轻则新客户端不收到功能，重则新旧端握手被硬相等校验直接拒绝。
    echo "::error::tag '${ref_name}' 指向归档目录 '${dir}'，但 ${manifest} 的版本是 ${toml_version}（应为 ${expected_dir}）" >&2
    echo "::error::请先把 ${manifest} 的 version 抬到 ${dir#v}，或者改推 ${expected_dir} 的 tag。" >&2
    exit 1
  fi
  # 手动指定了别的目录名（例如发一个 dev 包）：照发，但留痕
  echo "::warning::归档目录 '${dir}' 与 ${manifest} 的版本 ${toml_version} 不一致（手动指定，允许）" >&2
fi

echo "Resolved archive directory: ${dir} (source=${source}, ${manifest} version=${toml_version})"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "dir=${dir}"
    echo "version=${toml_version}"
    echo "source=${source}"
  } >>"${GITHUB_OUTPUT}"
else
  echo "dir=${dir}"
  echo "version=${toml_version}"
  echo "source=${source}"
fi
