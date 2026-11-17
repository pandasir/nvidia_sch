#!/usr/bin/env bash
#
# sync-gitee-release.sh — 把一次 tag 发布幂等地同步到 Gitee
#
# 用法:
#   GITEE_TOKEN=xxx scripts/sync-gitee-release.sh v0.1.0 \
#     --assets <文件或目录> [--assets ...] \
#     [--name "GPU Manager v0.1.0"] \
#     [--body-file <file> | --body "文本"] \
#     [--prerelease auto|true|false] \
#     [--target-commitish <ref>] \
#     [--dry-run]
#
# 环境变量:
#   GITEE_TOKEN     除 --dry-run 外必需。Gitee 私人令牌，需要 projects 权限。
#   GITEE_OWNER     除 --dry-run 外必需。仓库所属账号或组织，例如 yydb
#   GITEE_REPO      除 --dry-run 外必需。仓库名，例如 nvidia_sch
#   GITEE_API_BASE  可选，默认 https://gitee.com/api/v5
#   GITEE_RETRY_MAX 可选，429/5xx 最大尝试次数，默认 4
#   GITEE_UPLOAD_MAX_TIME 可选，单个附件上传的总超时秒数，默认 900
#                  （跨境上传慢时需要更长；低于 1KB/s 持续 60s 会主动放弃并重试）
#
# 行为:
#   1. 用令牌探测仓库可见性，区分「令牌无效」与「仓库不可见」
#   2. GET  releases/tags/<tag>：为 null 则创建，否则原地更新
#   3. GET  releases/<id>/attach_files：同名附件先删除再上传
#   4. 对 429 与 5xx 做有限次指数退避重试
#
# 幂等性: 重复执行不会产生重复 release，每个附件在 release 上只存在一份。
# 因此 CI 失败后本机重跑同一命令即可补齐，不需要重新构建。
#
# 依赖: bash 4+、curl、jq

set -euo pipefail

API_BASE="${GITEE_API_BASE:-https://gitee.com/api/v5}"
RETRY_MAX="${GITEE_RETRY_MAX:-4}"

TAG=""
ASSETS=()
NAME=""
BODY=""
BODY_FILE_INPUT=""
PRERELEASE="auto"
TARGET_COMMITISH=""
DRY_RUN=0

log() { printf '[sync-gitee] %s\n' "$*"; }
die() { printf '[sync-gitee] error: %s\n' "$*" >&2; exit 1; }

short_body() {
  tr -d '\r' < "$BODY_FILE_RESP" | tr '\n\t' '  ' | cut -c1-300
}

usage() {
  sed -n '3,32p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

# ---------------------------------------------------------------- 参数解析

if [ $# -eq 0 ]; then usage 1; fi

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --assets)           [ $# -ge 2 ] || die "--assets 需要一个参数"; ASSETS+=("$2"); shift 2 ;;
    --name)             [ $# -ge 2 ] || die "--name 需要一个参数"; NAME="$2"; shift 2 ;;
    --body)             [ $# -ge 2 ] || die "--body 需要一个参数"; BODY="$2"; shift 2 ;;
    --body-file)        [ $# -ge 2 ] || die "--body-file 需要一个参数"; BODY_FILE_INPUT="$2"; shift 2 ;;
    --prerelease)       [ $# -ge 2 ] || die "--prerelease 需要一个参数"; PRERELEASE="$2"; shift 2 ;;
    --target-commitish) [ $# -ge 2 ] || die "--target-commitish 需要一个参数"; TARGET_COMMITISH="$2"; shift 2 ;;
    --dry-run)          DRY_RUN=1; shift ;;
    -*)                 die "未知参数: $1" ;;
    *)                  [ -z "$TAG" ] || die "多余的参数: $1"; TAG="$1"; shift ;;
  esac
done

[ -n "$TAG" ] || die "缺少 tag 参数，例如: sync-gitee-release.sh v0.1.0 --assets out/"

case "$PRERELEASE" in
  auto|true|false) ;;
  *) die "--prerelease 只接受 auto|true|false，收到: $PRERELEASE" ;;
esac
if [ "$PRERELEASE" = "auto" ]; then
  if [[ "$TAG" =~ -(rc|alpha|beta|pre) ]]; then PRERELEASE=true; else PRERELEASE=false; fi
fi

# ---------------------------------------------------------------- 附件清单

FILES=()
for entry in "${ASSETS[@]:-}"; do
  [ -n "$entry" ] || continue
  if [ -d "$entry" ]; then
    while IFS= read -r f; do FILES+=("$f"); done < <(find "$entry" -maxdepth 1 -type f ! -name '.*' | sort)
  elif [ -f "$entry" ]; then
    FILES+=("$entry")
  else
    die "附件路径不存在: $entry"
  fi
done
[ ${#FILES[@]} -gt 0 ] || die "没有任何附件可上传，请用 --assets 指定文件或目录"

# release 描述默认取 tag 的 message
if [ -z "$BODY" ] && [ -z "$BODY_FILE_INPUT" ]; then
  if BODY="$(git tag -l --format='%(contents)' "$TAG" 2>/dev/null)"; then :; else BODY=""; fi
  while [ "${BODY%$'\n'}" != "$BODY" ]; do BODY="${BODY%$'\n'}"; done
  [ -n "${BODY//[[:space:]]/}" ] || BODY="-"
fi
if [ -n "$BODY_FILE_INPUT" ]; then
  [ -f "$BODY_FILE_INPUT" ] || die "--body-file 不存在: $BODY_FILE_INPUT"
  BODY="$(cat "$BODY_FILE_INPUT")"
fi
[ -n "$NAME" ] || NAME="$TAG"

if [ "$DRY_RUN" = "1" ]; then
  log "dry-run tag=$TAG name=$NAME prerelease=$PRERELEASE"
  log "dry-run 附件 (${#FILES[@]} 个):"
  for f in "${FILES[@]}"; do log "  - $f"; done
  log "dry-run 描述首行: $(printf '%s' "$BODY" | head -1)"
  log "dry-run 未发起任何请求"
  exit 0
fi

# ---------------------------------------------------------------- 前置校验

[ -n "${GITEE_TOKEN:-}" ] || die "缺少 GITEE_TOKEN。请在 Gitee「设置 → 私人令牌」生成 projects 权限令牌，并通过环境变量传入，不要写进任何文件"
[ -n "${GITEE_OWNER:-}" ] || die "缺少 GITEE_OWNER（仓库所属账号或组织）"
[ -n "${GITEE_REPO:-}"  ] || die "缺少 GITEE_REPO（仓库名）"
command -v curl >/dev/null || die "需要 curl"
command -v jq   >/dev/null || die "需要 jq"

REPO_PATH="/repos/${GITEE_OWNER}/${GITEE_REPO}"

# ---------------------------------------------------------------- HTTP 封装

STATUS=""
BODY_FILE_RESP=""

# http <METHOD> <PATH> [curl 额外参数...]
# 结果: STATUS 为 HTTP 状态码，BODY_FILE_RESP 为响应体文件
http() {
  local method="$1" path="$2"; shift 2
  local attempt=1 delay=1 code
  BODY_FILE_RESP="$(mktemp)"
  while :; do
    if ! code="$(curl -sS --max-time 120 -o "$BODY_FILE_RESP" -w '%{http_code}' \
        -X "$method" \
        -H "Authorization: Bearer ${GITEE_TOKEN}" \
        -H "Accept: application/json" \
        "$@" "${API_BASE}${path}")"; then
      rm -f "$BODY_FILE_RESP"
      die "网络请求失败: ${method} ${path}（检查网络或 api.gitee.com 可达性）"
    fi
    case "$code" in
      429|500|502|503|504)
        if [ "$attempt" -lt "$RETRY_MAX" ]; then
          log "HTTP ${code}，${delay}s 后重试 (${attempt}/${RETRY_MAX}): ${method} ${path}"
          sleep "$delay"; attempt=$((attempt + 1)); delay=$((delay * 2))
          continue
        fi
        ;;
    esac
    STATUS="$code"
    return 0
  done
}

json_get() { jq -r "$1" < "$BODY_FILE_RESP"; }

log "探测仓库 ${GITEE_OWNER}/${GITEE_REPO}"
http GET "$REPO_PATH"
case "$STATUS" in
  200) ;;
  401) die "Gitee 令牌无效或已过期 (HTTP 401): $(short_body)" ;;
  403) die "Gitee 拒绝该令牌访问 (HTTP 403): $(short_body)" ;;
  404) die "令牌无法访问仓库 ${GITEE_OWNER}/${GITEE_REPO} (HTTP 404)：仓库不存在，或该令牌所属账号对仓库没有权限: $(short_body)" ;;
  *)   die "探测仓库失败 (HTTP $STATUS): $(short_body)" ;;
esac

# ---------------------------------------------------------------- release

log "查询 tag ${TAG} 对应的 release"
http GET "${REPO_PATH}/releases/tags/${TAG}"

RELEASE_ID=""
if [ "$STATUS" = "200" ] && [ "$(json_get 'type' 2>/dev/null || echo null)" != "null" ]; then
  RELEASE_ID="$(json_get '.id')"
  log "release 已存在 (id=${RELEASE_ID})，将原地更新"
  # Gitee 的更新接口要求 tag_name 必填，缺了会返回 400 {"messages":["tag_name is missing"]}
  payload="$(jq -n --arg tag "$TAG" --arg name "$NAME" --arg body "$BODY" --argjson pre "$PRERELEASE" \
    '{tag_name: $tag, name: $name, body: $body, prerelease: $pre}')"
  http PATCH "${REPO_PATH}/releases/${RELEASE_ID}" \
    -H 'Content-Type: application/json' -d "$payload"
  [ "$STATUS" = "200" ] || die "更新 release 失败 (HTTP $STATUS): $(short_body)"
else
  log "release 不存在，创建中"
  payload="$(jq -n --arg tag "$TAG" --arg name "$NAME" --arg body "$BODY" \
    --arg target "$TARGET_COMMITISH" --argjson pre "$PRERELEASE" \
    '{tag_name: $tag, name: $name, body: $body, prerelease: $pre}
     + (if $target != "" then {target_commitish: $target} else {} end)')"
  http POST "${REPO_PATH}/releases" \
    -H 'Content-Type: application/json' -d "$payload"
  [ "$STATUS" = "201" ] || [ "$STATUS" = "200" ] || die "创建 release 失败 (HTTP $STATUS): $(short_body)"
  RELEASE_ID="$(json_get '.id')"
  [ "$RELEASE_ID" != "null" ] && [ -n "$RELEASE_ID" ] || die "创建 release 成功但未返回 id: $(short_body)"
  log "release 已创建 (id=${RELEASE_ID})"
fi

# ---------------------------------------------------------------- 现有附件

asset_id() { jq -r '.id // empty' <<<"$1"; }

log "读取现有附件清单"
EXISTING_JSON="[]"
page=1
while :; do
  http GET "${REPO_PATH}/releases/${RELEASE_ID}/attach_files?per_page=100&page=${page}"
  case "$STATUS" in
    200) ;;
    404) break ;;  # 无附件时 Gitee 可能直接返回 404
    *)   die "读取附件清单失败 (HTTP $STATUS): $(short_body)" ;;
  esac
  chunk="$(cat "$BODY_FILE_RESP")"
  [ "$(jq -r 'type' <<<"$chunk")" = "array" ] || die "附件清单返回非数组: $(short_body)"
  count="$(jq 'length' <<<"$chunk")"
  EXISTING_JSON="$(jq -s '.[0] + .[1]' <<<"$EXISTING_JSON
$chunk")"
  [ "$count" -ge 100 ] || break
  page=$((page + 1))
done

# ---------------------------------------------------------------- 上传

uploaded=0
replaced=0
for f in "${FILES[@]}"; do
  fname="$(basename "$f")"
  while :; do
    match="$(jq -c --arg n "$fname" '[.[] | select((.name // .file_name // .filename) == $n)] | first // empty' <<<"$EXISTING_JSON")"
    [ -n "$match" ] || break
    old_id="$(asset_id "$match")"
    [ -n "$old_id" ] || break
    log "删除同名旧附件 ${fname} (id=${old_id})"
    http DELETE "${REPO_PATH}/releases/${RELEASE_ID}/attach_files/${old_id}"
    case "$STATUS" in
      200|201|204) ;;
      *) die "删除旧附件失败 (HTTP $STATUS): $(short_body)" ;;
    esac
    replaced=$((replaced + 1))
    EXISTING_JSON="$(jq -c --arg n "$fname" '[.[] | select((.name // .file_name // .filename) != $n)]' <<<"$EXISTING_JSON")"
  done

  log "上传 ${fname} ($(wc -c < "$f" | tr -d ' ') 字节)"
  http POST "${REPO_PATH}/releases/${RELEASE_ID}/attach_files" \
    --max-time "${GITEE_UPLOAD_MAX_TIME:-900}" \
    --connect-timeout 20 \
    --speed-limit 1024 --speed-time 60 \
    --retry 3 --retry-delay 5 \
    -H 'Expect:' \
    -F "file=@${f}"
  case "$STATUS" in
    200|201) ;;
    401) die "上传附件时令牌无效 (HTTP 401): $(short_body)" ;;
    *)   die "上传 ${fname} 失败 (HTTP $STATUS): $(short_body)" ;;
  esac
  new_id="$(asset_id "$(cat "$BODY_FILE_RESP")")"
  [ -n "$new_id" ] || die "上传 ${fname} 后未返回附件 id: $(short_body)"
  EXISTING_JSON="$(jq -c --arg n "$fname" --argjson i "$new_id" '. + [{name: $n, id: $i}]' <<<"$EXISTING_JSON")"
  uploaded=$((uploaded + 1))
done

log "完成: release=${TAG} id=${RELEASE_ID} 上传=${uploaded} 替换=${replaced} 总附件=$(jq 'length' <<<"$EXISTING_JSON")"