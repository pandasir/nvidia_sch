# Tasks

## 1. Gitee 投递脚本

- [x] 1.1 新增 `scripts/sync-gitee-release.sh`：入参为 tag 与附件目录，先 `GET /repos/{owner}/{repo}/releases/tags/{tag}` 判定创建或更新，再逐附件上传；令牌从 `GITEE_TOKEN` 读取，绝不写入日志或文件。验证：`bash -n` 通过，`shellcheck` 无 error 级问题，缺少 `GITEE_TOKEN` 时以非零码退出并打印明确原因。
- [x] 1.2 实现附件幂等：上传前查询 `attach_files` 列表，同名附件先 `DELETE` 再上传。验证：对一个临时 tag 连跑两次脚本，两次之后 `attach_files` 返回的条目数与文件名集合完全相同。
- [x] 1.3 实现失败可诊断：每个请求失败时输出 HTTP 状态码与响应体摘要，并对 5xx/429 做有限次指数退避重试。验证：用错误令牌与不存在的仓库分别跑一次，输出分别指出认证失败与仓库不可见，退出码非零。
- [x] 1.4 在 `scripts/README.md`（或脚本头部注释）写明用法：CI 调用方式、本机补发命令、所需令牌权限。验证：按文档中的命令原样执行一次（对本机已有 tag 或 `--dry-run`）可跑通。

## 2. 发布 workflow

- [ ] 2.1 新增 `.github/workflows/release.yml`：触发为 `push` tag `v*` 与 `workflow_dispatch`（输入 tag），`permissions.contents: write`。验证：`act` 不可用时用 `python -c` + `yaml.safe_load` 校验语法，并在 GitHub 上以 `workflow_dispatch`（故意给不存在的 tag）跑一次确认能被调度。
- [x] 2.2 guard 阶段：校验 tag 与 `workspace.package.version` 相等，不等则以明确信息失败。验证：本地用 `v0.2.0` 与当前 `0.1.0` 手动跑一次校验逻辑，得到失败与非零码；用 `v0.1.0` 得到通过。
- [x] 2.3 guard 阶段：校验 Gitee 侧同名 tag 存在且指向同一 commit，有限轮询等待。验证：分别对「tag 已在 Gitee」「tag 不在 Gitee」两种状态各跑一次，前者通过、后者在超时后失败并提示先推送 tag。
- [ ] 2.4 build 阶段：`ubuntu-22.04` 上 `cargo test --workspace` 后 `cargo build --release --bins`，打包 `gpu-manager-linux-amd64-<ver>.tar.gz`（含 `gpu-daemon`、`gpu`、平台说明文件）并生成 `SHA256SUMS`，产物经 artifact 传递。验证：workflow 内 `tar -tzf` 列出预期文件，`sha256sum -c` 自校验通过，测试失败时该阶段失败且不产出 artifact。
- [ ] 2.5 github 阶段：`gh release create <tag> --verify-tag`，描述取 tag message，附 `tar.gz` 与 `SHA256SUMS`，tag 含 `-rc`/`-beta` 时加 `--prerelease`。验证：用 `v0.1.0-rc1` 与 `v0.1.0` 各发布一次，前者在 GitHub 上显示为 pre-release，后者为 latest，两者描述与 tag message 一致。
- [ ] 2.6 gitee 阶段：复用 1.x 脚本上传同一份 artifact，`needs` 于 build，失败不影响已完成的 GitHub release。验证：临时把 `GITEE_TOKEN` 指向无效值跑一次，GitHub release 仍在而该 job 标红；随后用正确令牌 `workflow_dispatch` 重跑，Gitee 侧被补齐且附件无重复。

## 3. 交付面迁移

- [x] 3.1 删除 `.gitignore` 中的 `dist` 条目，本地删除 `dist/` 目录。验证：`git status --porcelain` 干净，`git check-ignore -v dist/x` 不再命中。
- [ ] 3.2 改写 README 安装段：删除 `raw.githubusercontent.com/.../dist/...` 链接，改为 GitHub `releases/latest/download/` 与按版本下载两种写法，附 `sha256sum -c` 校验步骤，并补充 Gitee 镜像下载地址。验证：逐个测试文档中的 URL 返回 200/302 且能下载到与 `SHA256SUMS` 匹配的文件。
- [ ] 3.3 校核 README 中的 glibc 声明与产物实际要求：用 `objdump -T`（或 CI 内 `ldd --version`）确认 runner 产物所需的最低 glibc，把文档改为实测下限。验证：文档声明与实测结果一致，且不再出现未经验证的「2.17+」。

## 4. 一次性配置与首次发版

- [x] 4.1 为 `github` 远端追加 Gitee pushurl：`git remote set-url --add --push github git@gitee.com:yydb/nvidia_sch.git`。验证：`git remote -v` 显示 `github` 有两条 pushurl，`git push --dry-run github --follow-tags` 命中两个远端。
- [ ] 4.2 配置仓库 Secret 与 Variables：`GITEE_TOKEN`（`projects` 权限，yydb 签发）、`GITEE_OWNER=yydb`、`GITEE_REPO=nvidia_sch`。验证：`gh secret list` 与 `gh variable list` 中三个名称均存在，且 workflow 中未出现明文令牌。
- [ ] 4.3 打首个 tag `v0.1.0`（annotated，message 写变更说明）并 `git push --follow-tags`。验证：两个远端 `git ls-remote --tags` 均返回该 tag 且 commit 相同（`140b015`）。
- [ ] 4.4 首次发版后核对两个平台的 result：release 名、描述、prerelease 状态一致，附件文件名一致，两个平台下载到的文件 `sha256sum` 相同。验证：分别下载并对比哈希，与 `SHA256SUMS` 一致。
- [ ] 4.5 按 README 文档的安装步骤在一台干净环境（或容器）里实跑一次并启动 `gpu-daemon --provider null --fake-devices 1`。验证：`gpu-daemon --version` / `gpu --version` 输出与 tag 版本一致，daemon 能启动并响应 `gpu ls`。

## Workflow follow-up

- 首次发版与文档核对完成后归档 change：`/opsx-archive`。
- 归档后确认 `openspec/specs/release-delivery/spec.md` 已生成，且 `openspec validate --specs` 通过。