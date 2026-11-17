# Spec Delta

## Purpose

定义 tag 触发的版本发布行为：一次构建、两个代码托管平台同步发布同一份可校验的二进制产物，使使用者能通过稳定的下载地址获得明确的版本。

## ADDED Requirements

### Requirement: Version tag triggers publication

推送符合 `v<semver>` 格式的 annotated tag 时，系统 SHALL 自动发起一次发布，且该发布只针对被推送的 tag。

#### Scenario: Tag push starts a release

- **WHEN** 仓库收到 `v0.1.0` 的 tag 推送
- **THEN** 系统对该 tag 启动一次发布流程，产物与 release 均标注为 `v0.1.0`

#### Scenario: Branch push does not release

- **WHEN** 仅向 `master` 推送提交而没有新增 tag
- **THEN** 系统不创建任何 release

### Requirement: Release gate on version consistency

发布流程 MUST 在构建前校验 tag 名称与 `Cargo.toml` 中 `workspace.package.version` 一致，不一致时终止发布并保持两个平台均无新 release。

#### Scenario: Tag matches workspace version

- **WHEN** tag 为 `v0.1.0` 且 workspace 版本为 `0.1.0`
- **THEN** 校验通过，发布流程继续

#### Scenario: Tag does not match workspace version

- **WHEN** tag 为 `v0.2.0` 而 workspace 版本仍为 `0.1.0`
- **THEN** 校验失败，发布中止，不产生任何平台的 release

### Requirement: Release requires the tag on both platforms

发布流程 MUST 在发布 Gitee release 前确认 Gitee 仓库已存在同名 tag 且指向与 GitHub 相同的 commit；若尚未到达，系统 SHALL 在有限时间内重试，超时后终止发布并给出明确原因。

#### Scenario: Tag present on both platforms

- **WHEN** 同名 tag 在 GitHub 与 Gitee 指向同一 commit
- **THEN** 发布继续，Gitee release 关联该 commit

#### Scenario: Tag missing on Gitee

- **WHEN** 等待超时后 Gitee 仍不存在该 tag
- **THEN** Gitee 发布步骤失败，并在结果中说明「tag 尚未推送到 Gitee」

### Requirement: Artifact packaging

发布 MUST 生成一个 Linux amd64 可执行文件归档，包含 `gpu-daemon`、`gpu` 与该平台的说明文件，文件名 MUST 体现平台与版本，并 MUST 同时产出校验和清单。

#### Scenario: Archive contents and naming

- **WHEN** 发布 `v0.1.0`
- **THEN** 产出 `gpu-manager-linux-amd64-0.1.0.tar.gz` 与对应 `SHA256SUMS`
- **AND** 归档内可执行文件为 x86-64 Linux glibc 动态链接构建

#### Scenario: Test suite gates the artifact

- **WHEN** workspace 的测试失败
- **THEN** 不产出归档，且不发布任何平台的 release

### Requirement: Identical assets on both platforms

GitHub release 与 Gitee release MUST 使用同一次构建产出的同一份归档与校验和，系统 MUST NOT 为任一平台单独重新构建。

#### Scenario: Both platforms carry the same bytes

- **WHEN** 一次发布成功完成
- **THEN** 两个平台上的归档文件校验和相同

### Requirement: Publication is idempotent

同一 tag 的重复发布 MUST 复用或更新已有 release，并替换同名附件，不得产生重复 release 或重复附件。

#### Scenario: Re-running a failed publication

- **WHEN** 对 `v0.1.0` 重新发起发布，而该 tag 在 Gitee 已有 release
- **THEN** 已有 release 被更新，同名附件被替换，最终附件数量与重新上传前一致

#### Scenario: Interrupted publication leaves no duplicates

- **WHEN** 附件上传中途中断后重新发起一次发布
- **THEN** 每个附件在 release 上只存在一份

### Requirement: Failure of one platform does not revoke the other

Gitee 发布失败 MUST NOT 删除或撤回已发布的 GitHub release；失败 MUST 可通过按 tag 手工重跑恢复。

#### Scenario: Gitee step fails after GitHub success

- **WHEN** GitHub release 已创建而 Gitee 上传返回错误
- **THEN** GitHub release 与其附件保持可用，发布结果标记为失败

#### Scenario: Manual re-run for a tag

- **WHEN** 对失败的 `v0.1.0` 手工重跑发布
- **THEN** 缺失的 Gitee 部分被补齐，GitHub 侧保持原有内容

### Requirement: Release notes and prerelease marking

release 描述 MUST 取自该 tag 的 message；名称含预发布标记（如 `-rc`、`-beta`）的 tag MUST 被标记为预发布版本。

#### Scenario: Notes from tag message

- **WHEN** tag 的 message 为一段变更说明
- **THEN** 两个平台的 release 描述均为该段文字

#### Scenario: Prerelease tag

- **WHEN** 发布 `v0.2.0-rc1`
- **THEN** 两个平台的 release 均标记为预发布

### Requirement: Stable download entry points

release 附件 MUST 是唯一的二进制交付入口，且 MUST 提供不随版本变化的下载地址，文档 MUST NOT 再指向仓库内文件路径。

#### Scenario: Latest release download

- **WHEN** 使用者按文档中的最新版本地址下载
- **THEN** 获得当前最新正式版本的归档与校验和清单

#### Scenario: Stale in-repo path removed

- **WHEN** 安装文档被查阅
- **THEN** 文档中不存在指向仓库内 `dist/` 的下载链接