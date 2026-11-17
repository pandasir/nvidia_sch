# Design

## Context

- Rust workspace，5 个 crate（`gpu-api`、`gpu-core`、`nvmlx`、`gpu-daemon`、`gpu-cli`），edition 2024 / rust 1.85，版本由 `workspace.package.version` 统一管理，当前 `0.1.0`。
- 仓库无任何 CI 配置；`.github/` 不存在。发布历史为零：两个平台 `releases`、`tags` 均为空。
- 两个远端：`github`（`pandasir/nvidia_sch`，public）与 `origin`（`yydb/nvidia_sch`，Gitee，现为 public）。本地 upstream 只指向 `github`，两边曾发生 master SHA 漂移。
- 既有产物来自本机 arm64 macOS，用 `.tools/` 下锁定的 zig 0.13.0 交叉编译到 `x86_64-unknown-linux-gnu`；`.tools/` 与 `dist/` 均未纳入版本控制。README 曾据此声明 glibc 2.17+。
- 该二进制运行时需要动态加载 NVIDIA 提供的 `libnvidia-ml.so`，因此不能改用 musl 静态链接。

## Goals / Non-Goals

**Goals:**

- 一条命令完成发版：建 tag、推送、两个平台各自出现同名 release 与相同附件。
- 发布逻辑只有一份实现，CI 与人工补发共用同一脚本。
- 发布过程可重入：任何一步失败都可原地重跑，不产生重复对象。
- 交付入口稳定：使用者永远从 release 附件下载，仓库内不再存放二进制。

**Non-Goals:**

- 不做 macOS / arm64 产物（当前交付面只有 Linux amd64）。
- 不做 Gitee Go 流水线（见决策 2）。
- 不做依赖签名、SBOM、容器镜像发布。
- 不改动 gRPC、CLI、调度逻辑等运行时代码。

## Decisions

### 决策 1：GitHub Actions 作为唯一发布大脑，Gitee 由它调 API 投递

发布代码只需存在一处，构建只需发生一次，两个平台拿到同字节产物。

**备选：Gitee Go + GitHub Actions 双流水线各自发布。** 拒绝原因：两份构建定义会漂移；Gitee Go 的 Rust 环境要自行拼装自定义镜像；公开仓库的流水线配置与变量对所有人可见，一旦其 Release 插件在社区版不可用就只能把令牌写进公开可见的变量里；且两边各自构建会产生不同哈希，与「同字节」目标冲突。

### 决策 2：以 tag 为唯一发布触发，`workflow_dispatch` 只用于补发

tag 天然携带版本语义，可回溯、可补发、可删除重来。push 到 `master` 不触发发布，避免文档提交也产出 release。

`workflow_dispatch` 输入同一 tag 走完全相同路径，因此失败补发不需要任何额外分支。

**备选：push `master` 时读 `Cargo.toml` 自动 bump 并发版。** 拒绝原因：需要额外的版本自增规则与幂等跳过逻辑，且产物版本号与用户认知的 tag 脱节。

### 决策 3：发布前串联校验（版本一致 + 双端 tag 一致），任一不满足即中止

发布失败要早、要明确，而不是在 Gitee 侧留下半成品。两个校验都放在独立的 guard 阶段，位于构建之前，成本几乎为零。

Gitee tag 检查需要容忍推送与 CI 启动之间的时序差，采用有限轮询而非固定 `sleep`。

**备选：只在 Gitee 建 release 时隐式建 tag。** 拒绝原因：Gitee OpenAPI 的隐式建 tag 行为未在文档中明确（只在其 Gitee Go 插件文档里描述过），属于不可依赖的灰色地带；若它实际把 tag 建在了错误的 commit 上，会静默产生错版。

### 决策 4：构建回到 runner 原生 x86_64

runner 是 `ubuntu-22.04`，天生 x86_64 + glibc，比在本机 zip/zig 交叉链上维护一套工具链更可信、更可复现。发布不再依赖任何一台个人机器在线。

代价是 glibc 基线由 runner 决定（Ubuntu 22.04 对应 2.35），比当前声明支持到 2.17 的范围窄。若确有老发行版用户，改用容器镜像交叉编译即可，是个局部替换，不改变整体架构。

**备选：CI 里继续跑 zig cc 交叉编译指定 glibc 2.17。** 未采用的原因：需在 CI 中下载并固定 zig 版本、显式传 glibc 目标，构建时间与调试成本更高；在拿到「确有 2.17 需求」的证据前不值得。

### 决策 5：Gitee 投递抽成幂等脚本，CI 与本地共用

脚本以 tag 为唯一输入：查同 tag release（有则更新、无则创建），再逐附件上传，同名先删后传。CI 调用它，本机也能调用它补发，两条路径共享同一份语义。

幂等性来自「以远端现状为准再行动」，不依赖本地状态文件或构建号。

**备选：一次性脚本只用于 CI。** 拒绝原因：CI 跨境调用失败时，人只能重跑整个 workflow 从头构建，反馈周期长。

### 决策 6：多 pushurl 让一次推送覆盖两个远端

`git remote set-url --add --push github <gitee-url>` 之后，`git push --follow-tags` 一次写两个远端，tag 与分支同时到位，从机制上消除「tag 只推到一半」和再次 SHA 漂移的可能。

**备选：alias 连续两条 push。** 拒绝原因：仍依赖人记得执行，且第二条失败时第一条已生效，落入半同步状态。

**备选：发版脚本里显式两次 push。** 可接受，但与「忘记敲」的风险相比不如 pushurl 结构性可靠；发版脚本仍会 `ls-remote` 复核两端 SHA 作为兜底。

### 决策 7：release 描述取自 tag message，预发布由 tag 名判定

不额外维护 `CHANGELOG.md`，减少一处需要同步的真相。tag message 在 `git tag -a` 时即可写成一段说明，且天然与版本绑定。

tag 名含 `-rc` / `-beta` 等后缀时标记 prerelease，两个平台一致。

### 决策 8：交付产物从仓库目录迁到 release 附件

`dist/` 退役，`.gitignore` 中的 `dist` 条目同步删除（否则将来有人重建目录会静默不被跟踪）。README 的下载地址改为 `releases/latest/download/<固定文件名模式>`。

固定文件名模式（`gpu-manager-linux-amd64-<ver>.tar.gz`）里的版本号与「latest 稳定地址」是冲突的，因此文档中同时给出「最新版固定地址示例」与「按版本下载」两种写法，并在测试里核对实际资产名。

## Risks / Trade-offs

- [GitHub runner 到 `api.gitee.com` 偶发 403 或限流] → 有限次指数退避重试；失败后 workflow 标红但不影响 GitHub release；脚本可本机重跑，幂等保证不脏数据。
- [Gitee OpenAPI 行为变更或附件接口限制] → 附件约 8MB，远低于常规限制；脚本对每种失败打印响应体与状态码，便于定位；接口路径集中在脚本顶部便于替换。
- [令牌泄露面] → `GITEE_TOKEN` 只在需要的 step 内以 env 注入，不写入任何日志与文件；不使用 `set -x`；仓库为 public，故不把令牌放进任何仓库内配置。
- [多 pushurl 放大误操作] → `git push --force` 会同时作用到两个远端。这是刻意的对称行为；若只想推一边，需显式使用远端 URL。
- [glibc 基线抬到 2.35，与 README 原「2.17+」声明不符] → 发版任务中包含「校核 README 声明与产物实际 glibc 要求」，以 `objdump -T` 实测为准，宁可写明真实下限也不留错误承诺。
- [tag 与 `Cargo.toml` 版本不同步] → guard 阶段硬失败，并在错误信息里给出应执行的 bump 操作。
- [Gitee 没有 GitHub 的 `latest` 语义] → Gitee 侧文档地址按具体版本给出，并保留「私有期内仅成员可下」的历史说明不作为长期承诺。

## Migration Plan

1. 本地一次性配置：`pushurl` 追加 Gitee；`gh secret set GITEE_TOKEN`；`gh variable set GITEE_OWNER / GITEE_REPO`。
2. 合入本 change（workflow + 脚本 + README + `.gitignore`），此时尚未产生任何 release。
3. 打首个 tag `v0.1.0`（与当前 `Cargo.toml` 一致，无需 bump），`git push --follow-tags`。
4. 观察两个平台：release 出现、附件同名同哈希、`SHA256SUMS` 可校验。
5. 用文档中的地址实测一次安装流程，确认 README 与真实资产名一致。

**回滚**：删除两个平台该 tag 的 release 与 tag 即可，代码与运行时行为零影响；若要彻底停用发布，删除 `.github/workflows/release.yml` 即可，无持久化状态残留。

## Open Questions

- 是否仍需支持 glibc 2.17（CentOS 7 / Ubuntu 20.04）？如果需要，把构建步骤换成固定镜像的交叉编译即可，specs 与任务结构不变。
- Gitee 侧 README 是否需要单独定制（例如把 GitHub 专属链接换成 Gitee 域名）？当前判断不需要——GitHub 链接在 Gitee 上仍可用，只是速度不如本地镜像站。