# Proposal

## Why

项目目前没有任何发布流程：仓库里没有任何 tag，GitHub 与 Gitee 都没有 Release（两边 `releases` 接口均返回空数组）。唯一的预编译产物放在 `dist/`，而 `dist` 被 `.gitignore` 排除，因此 README 中让用户 `curl` 的 `raw.githubusercontent.com/.../dist/...tar.gz` 实际是 404。交付链断在「有二进制、但没有任何人能下载到」，用户也无法确认自己拿到的是哪个版本、有没有被篡改。

同一个仓库还维护 GitHub 与 Gitee 两个远端，历史上已经出现过两边 master SHA 漂移（Gitee 落后 2 个 commit）。若发布仍靠手工，版本、产物与两个平台的代码状态会持续脱节。

## What Changes

- 建立 **tag 驱动的发布流水线**：推送 annotated tag `v*` 触发 GitHub Actions，构建、测试、打包并发布；同时接受 `workflow_dispatch` 手工补发同一 tag。
- **单一构建、双平台发布**：GitHub Release 与 Gitee Release 使用同一份 `gpu-manager-linux-amd64-<ver>.tar.gz` 与 `SHA256SUMS`，由 GitHub Actions 调用 Gitee OpenAPI v5 上传附件。
- 新增**幂等的 Gitee 同步脚本**：按 tag 查找 release，存在则更新、不存在则创建；同名附件先删除再上传，因此 CI 与本地重跑都不会产生重复对象或半成品。
- 新增**发版前置校验**：tag 必须与 `Cargo.toml` 的 `workspace.package.version` 一致；Gitee 侧必须已存在同名 tag 且指向同一个 commit（带轮询等待，容忍推送与 CI 启动之间的时序差）。
- 交付方式**从仓库内 `dist/` 迁移到 Release 附件**：`dist/` 目录退役，README 安装段改指向 `releases/latest/download/...`。**BREAKING**：既有下载路径（`raw.githubusercontent.com/.../dist/...`）失效并删除，替换为 Release 附件地址。
- 一次性配置不再漂移：通过多个 `pushurl` 让一条 `git push` 同时写 GitHub 与 Gitee；`GITEE_TOKEN` 存入 GitHub Repository Secrets，仓库坐标存入 Repository Variables。

## Capabilities

### New Capabilities

- `release-delivery`: tag 触发的版本发布能力。涵盖版本一致性校验、发布构建与产物封装、GitHub 与 Gitee 双平台 Release 及附件发布、幂等重发与失败补救、以及对外交付地址（下载入口、校验和）。

### Modified Capabilities

无。项目当前没有任何既有 spec，本次不修改既有需求。

## Impact

- 新增文件：`.github/workflows/release.yml`、`scripts/sync-gitee-release.sh`
- 修改文件：`README.md`（安装与升级段、下载地址）、`.gitignore`（移除 `dist` 条目）、`Cargo.toml`（发版时随 tag bump 版本号）
- 退役：`dist/` 目录（当前未纳入版本控制，无需提交删除）
- 外部依赖：GitHub Actions（`ubuntu-22.04` runner）、Gitee OpenAPI v5（`releases` 与 `attach_files`）、`GITEE_TOKEN`（`projects` 权限）、`gh` CLI
- 运行环境变化：发布构建从本机 macOS + zig 交叉编译改为 runner 上的 x86_64 原生 glibc 构建；本机 zig 链路不再参与发版，仅保留为离线兜底
- 不加新 crate 依赖，不改动任何 gRPC/CLI 行为，不影响运行时代码