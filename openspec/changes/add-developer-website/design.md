## Context

仓库已有完整的 Rust workspace、README、DESIGN 和 tag 驱动的 GitHub/Gitee 双端发布，但没有任何站点资源。现有 `release-delivery` 规范已经要求稳定下载地址，并要求文档不再指向仓库内构建目录。

本次官网面向已经决定“下载并试用”的开发者。官网不需要承载全部长期文档，而要把首次试用路径收拢到首页、下载页和快速开始页三个入口。

## Goals / Non-Goals

**Goals:**

- 在 GitHub Pages 子路径提供公开可访问的静态官网
- 让下载页准确呈现当前稳定版本、双平台来源、校验方式与平台兼容信息
- 让发布数据成为官网版本信息的唯一来源，避免手工同步
- 提供可在无 NVIDIA GPU 环境完成的快速体验路径
- 保持现有 Release 产物和发布语义不变

**Non-Goals:**

- 不迁移 README 中全部配置、部署、安全和排障内容
- 不引入后端、数据库、登录、遥测或在线 Playground
- 不使用自定义域名
- 不启用 Gitee Pages
- 不改变 Rust crate、gRPC API 或 CLI 行为

## Decisions

### 1. 使用 VitePress 构建静态站

选择 VitePress：它原生支持 Markdown、内置搜索、首页定制成本低，且适合未来逐步吸收 README 内容。

备选方案：

- Astro/Starlight：交互自由度更高，但初始复杂度超出当前范围
- MkDocs Material：文档体验好，产品首页表达力不足
- 手写 HTML：首页灵活，但文档和示例容易与 README 双份维护

站点源码放在 `docs/`，与 Rust crate 分离。

### 2. 部署到 GitHub Pages 子路径

部署地址为 `https://pandasir.github.io/nvidia_sch/`，VitePress `base` 固定为 `/nvidia_sch/`。

选择 GitHub Actions 部署而不是 `gh-pages` 分支：源码继续使用 `master` 作为唯一事实来源，部署产物由工作流生成。GitHub Pages 的 Source 设置为 GitHub Actions。

备选方案：

- 自定义域名：增加 DNS、证书和续期维护，暂不需要
- Gitee Pages：增加另一套部署链路和审核约束，国内入口已有 Release 镜像
- Cloudflare Pages/Vercel：访问体验可能更好，但引入外部平台依赖

### 3. 站点信息架构固定为三层

```text
/                       首页：价值、演示、架构、边界
/download               下载：版本、来源、校验、平台
/guide/quick-start/     快速开始：无卡体验、真机路径
```

首页导航只保留下载、快速开始、文档入口和两个代码仓库入口。README 保留完整技术说明，但安装与试用部分链接到官网，避免一次性搬迁所有文档。

### 4. 使用版本清单连接发布与下载页

站点运行时读取 `docs/public/release.json`。清单至少包含：

```json
{
  "version": "0.1.0",
  "tag": "v0.1.0",
  "prerelease": false,
  "github": {
    "archive": "https://github.com/pandasir/nvidia_sch/releases/download/v0.1.0/gpu-manager-linux-amd64-0.1.0.tar.gz",
    "latest": "https://github.com/pandasir/nvidia_sch/releases/latest/download/gpu-manager-linux-amd64.tar.gz"
  },
  "gitee": {
    "archive": "https://gitee.com/yydb/nvidia_sch/releases/download/v0.1.0/gpu-manager-linux-amd64-0.1.0.tar.gz",
    "available": true
  },
  "sha256sums": "https://github.com/pandasir/nvidia_sch/releases/download/v0.1.0/SHA256SUMS"
}
```

版本清单由发布流程在默认分支更新，只有语义版本更高的正式 release 才能替换当前稳定版本。预发布版本不替换稳定版本。Gitee 发布失败时，清单将 `gitee.available` 标记为 `false`，下载页显示同步中而不是死链。

由于 `GITHUB_TOKEN` 产生的提交不会自动触发其他工作流，发布流程更新清单后 MUST 显式调用站点部署工作流。部署工作流同时支持手动触发，便于补齐历史版本数据。

备选方案：

- 站点构建时直接查询 GitHub/Gitee API：减少仓库写入，但受匿名 API 限流和网络失败影响
- 手工编辑 `release.json`：实现最简单，但版本漂移风险高

### 5. 下载页以“可复现”为默认

页面默认展示带版本号的固定归档和对应 `SHA256SUMS`，GitHub latest 固定地址作为次级入口。Gitee 没有 latest 语义，因此只展示按版本下载地址。

下载按钮旁直接显示平台要求、校验命令和下一步入口，不要求访客跳转到 Release 页才能判断该下载什么。

### 6. 快速开始以无 GPU 路径为主

首页和快速开始页优先展示 Null Provider 演示，因为它可以让绝大多数访客立即验证调度闭环。真实 GPU 路径作为并列选项，说明驱动、`libnvidia-ml.so`、cgroup v2 和 systemd 要求。

命令示例使用仓库中实际存在的 CLI 参数，不使用伪命令或尚未实现的子命令。

## Risks / Trade-offs

- [GitHub Pages 子路径导致静态资源 404] → VitePress `base` 固定为 `/nvidia_sch/`，部署后检查首页、下载页和图片资源
- [官网版本与 Release 漂移] → 版本清单由发布流程生成，校验版本的语义顺序，并在站点构建时验证清单结构
- [Gitee 镜像失败导致死链] → 清单记录可用性，下载页按状态降级展示
- [GITHUB_TOKEN 提交不触发部署] → 发布流程显式调用可复用的部署工作流
- [预发布版本被误当成稳定版] → 只有更高的非预发布版本更新当前稳定版
- [README 与官网内容重复] → README 保留完整技术文档，官网只维护首页、下载页和快速开始页；长文档通过链接连接
- [macOS 用户下载 Linux 二进制] → 页面明确标注平台范围，并提供源码构建入口

## Migration Plan

1. 先加入 VitePress 站点骨架和三个页面，以当前 `v0.1.0` 数据作为初始版本清单
2. 加入 GitHub Pages 部署工作流，并在 GitHub Pages 设置中选择 GitHub Actions 作为来源
3. 验证子路径下的页面、链接、搜索和资源加载
4. 修改 README 的安装与快速开始入口，使其指向官网
5. 修改发布流程，在成功发布后更新版本清单并触发站点部署
6. 用现有 `v0.1.0` 手工触发站点部署，核对 GitHub/Gitee 下载入口、校验和命令和无卡演示路径
7. 回滚时恢复 README 链接并停用 Pages 工作流；已发布的 Release 和 Rust 代码不受影响
