## Purpose

为首次接触 GPU Manager 的开发者提供可公开访问的静态官网与下载路径，使其无需先读完完整设计文档，就能理解项目价值、选择正确产物、完成校验并跑通最小试用。

## ADDED Requirements

### Requirement: Public website entry point

系统 SHALL 在 `https://pandasir.github.io/nvidia_sch/` 提供公开可访问的官网首页。首页 MUST 提供通往下载页、快速开始页、GitHub 仓库与 Gitee 仓库的导航入口。

#### Scenario: Visitor opens the website

- **WHEN** 访客打开官网首页地址
- **THEN** 页面成功加载
- **AND** 访客能看到下载、快速开始、GitHub 和 Gitee 入口

### Requirement: Trial-oriented homepage

首页 SHALL 在首屏说明 GPU Manager 是单机整卡 GPU 调度与隔离服务，并明确指出无需 NVIDIA GPU 也能试用。首页的主行动 SHALL 指向下载页。

#### Scenario: Visitor evaluates the project

- **WHEN** 访客查看首页首屏
- **THEN** 能识别产品定位为单机 GPU 调度与隔离
- **AND** 能看到无 GPU 试用说明
- **AND** 能通过一个主按钮进入下载页

### Requirement: Download page version consistency

下载页 SHALL 展示一个当前稳定版本及其对应 tag，并 MUST 保证该版本下的归档地址、latest 地址与校验和链接互相匹配。页面 MUST NOT 指向仓库内构建目录中的产物路径。

#### Scenario: Current stable release is displayed

- **WHEN** 当前稳定版本为 `v0.1.0`
- **THEN** 下载页显示版本 `0.1.0` 和 tag `v0.1.0`
- **AND** 固定版本归档、latest 归档与 `SHA256SUMS` 均指向该发布
- **AND** 页面中不存在指向仓库内 `dist/` 的下载链接

#### Scenario: A newer stable release becomes current

- **WHEN** 更新的正式版本发布并同步到官网数据后
- **THEN** 下载页显示新版本及其对应下载信息
- **AND** 不再把旧版本标为当前稳定版

### Requirement: Dual download sources with explicit availability

下载页 SHALL 同时提供 GitHub 和 Gitee 两个下载入口。对于同一版本，两个入口 MUST 指向相同发布内容；当其中一个镜像不可用时，页面 MUST 明确显示不可用或同步中状态，且 MUST NOT 提供会导致失败的下载按钮。

#### Scenario: Both sources are available

- **WHEN** 当前版本的 GitHub Release 与 Gitee Release 均可用
- **THEN** 下载页同时显示两个下载按钮
- **AND** 两个按钮均指向当前版本的归档文件

#### Scenario: Gitee mirror is unavailable

- **WHEN** 当前版本的 Gitee 镜像尚未发布或被标记为不可用
- **THEN** 下载页显示 Gitee 镜像同步中或不可用
- **AND** Gitee 入口不作为可用下载按钮呈现

### Requirement: Platform-specific download guidance

下载页 SHALL 明确区分预编译产物的平台范围与源码构建路径。预编译二进制 MUST 标注为 Linux x86-64，并 MUST 标注 glibc 最低版本要求；其他平台 MUST 被引导到源码构建说明。

#### Scenario: Linux amd64 visitor selects a binary

- **WHEN** 访客在 Linux x86-64 环境查看下载页
- **THEN** 页面突出显示预编译归档
- **AND** 页面显示 glibc 最低版本为 2.34

#### Scenario: Non-Linux visitor needs a build path

- **WHEN** 访客在 macOS 或其他非 Linux x86-64 环境查看下载页
- **THEN** 页面提供源码构建入口
- **AND** 页面不把 Linux 预编译包描述为该平台的推荐安装方式

### Requirement: Checksum verification on the download path

下载页 SHALL 直接展示校验和文件入口与可复制的校验命令，使访客无需离开下载流程即可验证归档完整性。

#### Scenario: Visitor verifies an archive

- **WHEN** 访客下载归档并查看下载页的校验步骤
- **THEN** 能找到当前版本对应的 `SHA256SUMS`
- **AND** 能复制使用 `sha256sum -c` 的校验命令

### Requirement: Runnable quick-start paths

快速开始页 SHALL 提供 Null Provider 无卡体验路径和真实 GPU 路径。无卡体验路径 MUST 不依赖 NVIDIA 驱动，并 MUST 包含启动 daemon、查看设备、提交作业和查看作业的命令。

#### Scenario: Visitor tries without a GPU

- **WHEN** 访客按无卡体验路径操作
- **THEN** 可以使用 Null Provider 启动模拟设备
- **AND** 可以完成设备查看、作业提交与作业列表查看
- **AND** 过程中不要求安装 NVIDIA 驱动

#### Scenario: Visitor prepares a real GPU host

- **WHEN** 访客选择真实 GPU 路径
- **THEN** 页面说明 NVIDIA 驱动与 `libnvidia-ml.so` 要求
- **AND** 页面给出与无卡演示区分的 daemon 启动参数

### Requirement: Accurate scope boundaries

官网 SHALL 明确说明 v1 的单机、整卡独占范围，并 MUST NOT 暗示 MIG、MPS、多机调度或抢占已经可用。对于已设计但尚未实现的能力，官网 MUST 将其标记为未落地或路线图内容。

#### Scenario: Visitor compares capabilities

- **WHEN** 访客查看官网的能力说明或路线图
- **THEN** 能看到单机与整卡独占的范围边界
- **AND** 未实现的扩展能力不会被描述为当前可用功能
