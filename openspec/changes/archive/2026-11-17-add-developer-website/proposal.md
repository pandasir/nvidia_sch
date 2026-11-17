## Why

仓库已有完整的功能文档和 v0.1.0 双平台发布，但首次接触项目的开发者仍需要从 README 中自行拼出“这是什么、能否试用、下载哪个包、如何校验、怎样跑通第一次调度”的路径。官网应把这条路径压缩成一次清晰的下载与试用体验。

## What Changes

- 新增 VitePress 静态官网，部署到 GitHub Pages 子路径 `https://pandasir.github.io/nvidia_sch/`
- 新增首页，以“下载并试用”为目标，呈现项目价值、无 GPU 演示、架构说明和已知边界
- 新增 `/download` 页面，展示当前稳定版本、GitHub 与 Gitee 下载入口、固定版本与 latest 地址、`SHA256SUMS` 校验、平台兼容矩阵和源码构建路径
- 新增快速开始页面，分别覆盖 Null Provider 无卡体验与真实 GPU 的最短试用路径
- 由发布流程生成站点使用的版本清单，避免官网手工维护版本号和下载链接；Gitee 镜像不可用时明确显示同步状态，不提供死链
- README 保持精简并链接到官网；配置、部署、安全与排障文档暂不整体迁移
- 不包含后端服务、登录、遥测、在线 Playground、博客、自定义域名或 Gitee Pages

## Capabilities

### New Capabilities

- `developer-website`: 面向开发者的静态官网能力，包括首页价值说明、下载入口、版本清单、快速试用路径与 GitHub Pages 发布

### Modified Capabilities

- 无

## Impact

- 新增 `docs/` 站点源码与 VitePress 配置
- 新增站点部署工作流，并由现有发布流程提供版本清单
- 修改 README，使官网成为下载和快速试用的首选入口
- 不改变 Rust crate、gRPC API、CLI 参数或运行时行为
- 不改变 GitHub/Gitee Release 的既有产物；Gitee 仍是按版本下载的镜像入口
