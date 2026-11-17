# scripts/

发布流程用到的脚本。全部只读远端、可重复执行，失败后原地重跑即可。

## sync-gitee-release.sh

把一次 tag 发布幂等地同步到 Gitee：release 不存在则创建，存在则原地更新名称/描述/预发布标记；附件按文件名对比，同名先删旧再传新。因此重复执行不会产生重复 release 或重复附件。

### 前置

- 一个 Gitee 私人令牌（`Gitee → 设置 → 私人令牌`），勾选 `projects` 权限；
- 令牌所有者对该仓库有写权限；
- `curl`、`jq`（GitHub Actions 的 `ubuntu-*` runner 自带）。

令牌只通过环境变量传入，脚本不会写入文件、也不会回显。不要 `export GITEE_TOKEN` 到会记录命令历史的脚本里。

### 在 CI 中调用

```bash
GITEE_TOKEN="$GITEE_TOKEN" \
GITEE_OWNER="$GITEE_OWNER" \
GITEE_REPO="$GITEE_REPO" \
./scripts/sync-gitee-release.sh "${TAG}" \
  --assets dist-assets/gpu-manager-linux-amd64-"${VERSION}".tar.gz \
  --assets dist-assets/SHA256SUMS \
  --name "GPU Manager ${TAG}" \
  --target-commitish "${GITHUB_SHA}"
```

描述默认取该 tag 的 message（`git tag -l --format='%(contents)'`），预发布标记默认由 tag 名是否含 `-rc`/`-alpha`/`-beta`/`-pre` 判定。

### 本机补发

CI 的 Gitee 步骤失败时，不需要重新构建，直接补齐：

```bash
GITEE_TOKEN=xxx GITEE_OWNER=yydb GITEE_REPO=nvidia_sch \
  ./scripts/sync-gitee-release.sh v0.1.0 \
  --assets /path/to/gpu-manager-linux-amd64-0.1.0.tar.gz \
  --assets /path/to/SHA256SUMS
```

附件可以从 GitHub release 页面直接下载后传入。同一份产物、同一个 tag，重跑到成功为止都不会产生脏数据。

### 只检查不连网

`--dry-run` 会打印将要创建的 release 与待上传附件清单，不发任何请求，也不需要令牌：

```bash
./scripts/sync-gitee-release.sh v0.1.0 --assets dist-assets/ --dry-run
```

### 常见退出信息

| 输出 | 含义 |
|---|---|
| `缺少 GITEE_TOKEN` | 未传令牌。令牌不要写进仓库任何文件 |
| `令牌无效或已过期 (HTTP 401)` | 令牌被撤销或过期，或格式不对 |
| `令牌无法访问仓库 (HTTP 404)` | 仓库不存在，或令牌所属账号不是仓库成员 |
| `上传 xxx 失败 (HTTP 4xx/5xx)` | 附件接口拒绝，响应体摘要会一并打印 |

`429` 与 `5xx` 会自动指数退避重试（默认 4 次，可用 `GITEE_RETRY_MAX` 调整）。其余错误直接失败并打印响应体摘要，方便判断是权限问题还是接口问题。

### 跨境上传的时间参数

从境外 runner 往 `api.gitee.com` 传附件时，慢速链路是主要风险（实测 GitHub runner 上 3.9MB 的归档在默认 120s 超时内传不完）。因此单个附件上传单独放宽为：

| 参数 | 默认 | 含义 |
|---|---|---|
| `GITEE_UPLOAD_MAX_TIME` | `900` | 单个附件上传的总超时秒数 |
| 硬编码 | `20` | 连接超时 |
| 硬编码 | `1024` B/s × 60s | 低于该速度持续 60s 即放弃，交给 curl 重试 |

上传失败不会留下脏数据：脚本下次运行会按文件名比对，同名附件先删后传。如果一个附件始终传不上去，可在国内机器上直接重跑同一命令（同 tag、同附件），不需要重新构建。