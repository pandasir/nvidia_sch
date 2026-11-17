# GPU Manager

GPU Manager 是一套运行在单台机器上的 NVIDIA GPU 调度与分配服务。它以**整卡独占**为粒度，负责设备发现、状态监控、作业排队、进程启动、资源回收和审计。客户端通过 Unix Domain Socket 调用本地 gRPC API， daemon 根据分配结果注入 `CUDA_VISIBLE_DEVICES`，并可通过 cgroup v2 对设备节点进行硬隔离。

## 功能概览

- 通过 NVML 枚举和监控真实 GPU；也可使用 Null Provider 在无 GPU 环境中运行
- 按整卡分配资源，资源不足时按优先级排队
- 自动拉起作业进程，并在进程退出、被终止或设备故障后释放设备
- 以 UDS 对端凭证识别调用者，普通用户只能查看自己的作业
- daemon 以 root 运行时，将作业进程降权到提交者身份
- 可选 cgroup v2 设备过滤，限制作业只能访问分配给它的 GPU
- JSONL 审计日志、Prometheus 指标、状态文件重启恢复
- 可配置每用户 GPU 在飞数量和并发作业数量限制

## 组件

| 组件 | 说明 |
|---|---|
| `gpu-daemon` | 守护进程：设备监控、调度、gRPC API、作业编排、强制层、审计和指标 |
| `gpu` | 命令行客户端：`ls`、`watch`、`run`、`jobs`、`queue`、`kill` |
| `gpu-core` | 调度状态和领域模型 |
| `gpu-api` | `proto/gpu.proto` 及 tonic/prost 生成代码 |
| `nvmlx` | NVML 真机后端和 Null Provider 模拟后端 |

## 环境要求

### 源码构建

- Rust 1.85 或更高版本（项目使用 edition 2024）
- protoc 无需单独安装，构建脚本会使用 vendored 版本
- Linux、macOS 均可编译；macOS 适合使用 Null Provider 开发和演示

### Linux 生产部署

- x86-64 Linux 与 glibc；Release 提供预编译的 amd64 二进制（在 Ubuntu 22.04 runner 上原生构建）
- 真机 GPU 模式需要正常安装的 NVIDIA 驱动和 `libnvidia-ml.so`
- 需要硬隔离和进程降权时，daemon 应以 root 运行，并具备 cgroup v2 写权限
- 推荐 systemd 管理 daemon
- 多用户访问 socket 时需要安装 `acl` 包

无 GPU 机器可使用 `--provider null --fake-devices N` 运行完整调度流程，不需要 NVIDIA 驱动。

默认路径：

| 项目 | Linux | 其他平台 |
|---|---|---|
| gRPC socket | `/run/gpu-manager/gpu.sock` | `/tmp/gpu-manager/gpu.sock` |
| Prometheus 指标 | `http://127.0.0.1:9900/metrics` | `http://127.0.0.1:9900/metrics` |

## 安装

### 使用预编译包

每个 tag 的产物由 CI 构建并发布到 GitHub Release 与 Gitee 镜像：`gpu-daemon`、`gpu` 与平台说明文件打成一个 tar.gz，并附 `SHA256SUMS`。

最新版本（不带版本号的文件名，指向当前最新正式 release）：

```bash
curl -fL -O https://github.com/pandasir/nvidia_sch/releases/latest/download/gpu-manager-linux-amd64.tar.gz
curl -fL -O https://github.com/pandasir/nvidia_sch/releases/latest/download/SHA256SUMS

sha256sum -c SHA256SUMS --ignore-missing
tar -xzf gpu-manager-linux-amd64.tar.gz

sudo install -d -m 0755 /opt/gpu-manager
sudo install -m 0755 linux-amd64/gpu-daemon /opt/gpu-manager/gpu-daemon
sudo install -m 0755 linux-amd64/gpu /opt/gpu-manager/gpu
```

指定版本：

```bash
VERSION=0.1.0

curl -fL -O "https://github.com/pandasir/nvidia_sch/releases/download/v${VERSION}/gpu-manager-linux-amd64-${VERSION}.tar.gz"
curl -fL -O "https://github.com/pandasir/nvidia_sch/releases/download/v${VERSION}/SHA256SUMS"

sha256sum -c SHA256SUMS --ignore-missing
tar -xzf "gpu-manager-linux-amd64-${VERSION}.tar.gz"

sudo install -d -m 0755 /opt/gpu-manager
sudo install -m 0755 linux-amd64/gpu-daemon /opt/gpu-manager/gpu-daemon
sudo install -m 0755 linux-amd64/gpu /opt/gpu-manager/gpu
```

`SHA256SUMS` 同时列出带版本号与不带版本号的两个归档，只下载其中一个时用 `--ignore-missing` 校验已下载的那个。

国内可从 Gitee 镜像下载同一份产物（Gitee 没有 `latest` 语义，按版本取）：

```bash
VERSION=0.1.0

curl -fL -O "https://gitee.com/yydb/nvidia_sch/releases/download/v${VERSION}/gpu-manager-linux-amd64-${VERSION}.tar.gz"
curl -fL -O "https://gitee.com/yydb/nvidia_sch/releases/download/v${VERSION}/SHA256SUMS"

sha256sum -c SHA256SUMS --ignore-missing
tar -xzf "gpu-manager-linux-amd64-${VERSION}.tar.gz"
```

验证安装：

```bash
/opt/gpu-manager/gpu-daemon --version
/opt/gpu-manager/gpu --version
```

版本号与 tag 一致：发布 `v0.1.0` 时两个命令都输出 `0.1.0`。

### 从源码安装

```bash
git clone https://github.com/pandasir/nvidia_sch.git
cd nvidia_sch

cargo build --release --workspace --bins
cargo test --workspace

sudo install -d -m 0755 /opt/gpu-manager
sudo install -m 0755 \
  target/release/gpu-daemon \
  target/release/gpu \
  /opt/gpu-manager/
```

### 在 macOS 上交叉编译 Linux amd64

正式发布产物由 GitHub Actions 在 `ubuntu-22.04` 上原生构建（见 `.github/workflows/release.yml`）。需要在没有 CI 的机器上自行出二进制时，可以用下面这条链路：

产物使用 glibc 动态链接，能够在主流 Linux 发行版上运行：

```bash
rustup target add x86_64-unknown-linux-gnu

export ZIG_GLOBAL_CACHE_DIR="$PWD/.tools/zig-cache/global"
export ZIG_LOCAL_CACHE_DIR="$PWD/.tools/zig-cache/local"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/.tools/zig-macos-aarch64-0.13.0/zig"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-target -C link-arg=x86_64-linux-gnu"

cargo build --release --target x86_64-unknown-linux-gnu --workspace --bins
```

产物位于：

```text
target/x86_64-unknown-linux-gnu/release/gpu-daemon
target/x86_64-unknown-linux-gnu/release/gpu
```

选择 glibc 而不是 musl，是为了让 daemon 在运行时加载 NVIDIA 驱动提供的 `libnvidia-ml.so`。

## daemon 配置

可通过 `gpu-daemon --help` 查看完整参数。

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--socket-path <path>` | Linux `/run/gpu-manager/gpu.sock` | gRPC UDS 路径 |
| `--provider <null\|nvml>` | `null` | 设备发现后端 |
| `--fake-devices <n>` | `0` | Null Provider 的模拟卡数，仅对 `null` 生效 |
| `--poll-interval-secs <n>` | `5` | 设备状态轮询间隔 |
| `--metrics-port <port>` | `9900` | Prometheus 指标端口，`0` 表示关闭 |
| `--queue-timeout-secs <n>` | `3600` | 排队超时秒数，`0` 表示不限 |
| `--audit-log <path>` | 关闭 | JSONL 审计日志 |
| `--cgroup-mode <auto\|v2\|noop>` | `auto` | `v2` 强制硬隔离，`auto` 探测失败后降级，`noop` 关闭隔离 |
| `--state-file <path>` | 关闭 | 状态持久化与重启恢复文件 |
| `--max-gpus-per-user <n>` | `0` | 每用户在飞 GPU 数上限，`0` 表示不限 |
| `--max-concurrent-jobs-per-user <n>` | `0` | 每用户非终态作业数上限，`0` 表示不限 |

日志级别通过 `RUST_LOG` 控制，例如：

```bash
RUST_LOG=info gpu-daemon --provider nvml
RUST_LOG=gpu_daemon=debug gpu-daemon --provider nvml
```

## 快速开始：无 GPU 演示

启动 daemon：

```bash
mkdir -p /tmp/gpu-manager

/opt/gpu-manager/gpu-daemon \
  --socket-path /tmp/gpu-manager/gpu.sock \
  --provider null \
  --fake-devices 3 \
  --poll-interval-secs 2 \
  --cgroup-mode noop \
  --audit-log /tmp/gpu-manager/audit.jsonl \
  --state-file /tmp/gpu-manager/state.json
```

在另一个终端使用客户端：

```bash
SOCK=/tmp/gpu-manager/gpu.sock

# 查看设备
gpu --socket-path "$SOCK" ls

# 订阅设备状态变化，Ctrl-C 退出
gpu --socket-path "$SOCK" watch

# 提交一个需要两张卡的任务
gpu --socket-path "$SOCK" run \
  --gpus=2 \
  --description="demo" \
  --priority=5 \
  -- sleep 30

# 查看任务与队列
gpu --socket-path "$SOCK" jobs
gpu --socket-path "$SOCK" queue

# 终止已分配或运行中的任务
gpu --socket-path "$SOCK" kill <job_id>

# 查看审计日志和指标
tail -f /tmp/gpu-manager/audit.jsonl
curl -s http://127.0.0.1:9900/metrics
```

`gpu run` 会阻塞观察任务直到终态。按 Ctrl-C 只会退出观察，任务会继续在后台运行，可使用 `gpu jobs` 查看。CLI 的 `kill` 用于已分配或运行中的任务；gRPC 另提供 `CancelJob` 用于排队任务。

建议为可执行文件使用绝对路径。作业进程由 daemon 启动，其 `PATH` 和工作目录可能与当前 shell 不同。

## 生产部署

### 主机准备

```bash
sudo install -d -m 0755 /var/log/gpu-manager /var/lib/gpu-manager

# 多用户访问时需要 acl
sudo apt-get install -y acl       # Debian/Ubuntu
sudo dnf install -y acl           # Fedora/RHEL

# 创建允许访问 daemon socket 的用户组
sudo groupadd --system gpu-users
sudo usermod -aG gpu-users alice
```

用户加入 `gpu-users` 后需要重新登录才能生效。

检查真机依赖：

```bash
nvidia-smi
mount | grep cgroup2
```

`--cgroup-mode v2` 在 cgroup v2 不可用时会让 daemon 启动失败；如需自动降级，可使用 `auto`。`noop` 只依靠 `CUDA_VISIBLE_DEVICES` 软约束，不适合作为多用户主机上的硬隔离方案。

### systemd 服务

创建 `/etc/systemd/system/gpu-manager.service`：

```ini
[Unit]
Description=GPU Manager Daemon
After=local-fs.target

[Service]
Type=simple
User=root
Environment=RUST_LOG=info
RuntimeDirectory=gpu-manager
RuntimeDirectoryMode=0755
ExecStart=/opt/gpu-manager/gpu-daemon \
  --socket-path /run/gpu-manager/gpu.sock \
  --provider nvml \
  --poll-interval-secs 5 \
  --metrics-port 9900 \
  --queue-timeout-secs 3600 \
  --audit-log /var/log/gpu-manager/audit.jsonl \
  --state-file /var/lib/gpu-manager/state.json \
  --cgroup-mode v2 \
  --max-gpus-per-user 4 \
  --max-concurrent-jobs-per-user 8
ExecStartPost=/usr/bin/timeout 5 /bin/sh -c 'while [ ! -S /run/gpu-manager/gpu.sock ]; do sleep 0.1; done; /usr/bin/setfacl -m g:gpu-users:rw /run/gpu-manager/gpu.sock'
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

示例中的配额可按需要调整；删除对应参数则表示不限制。加载并启动服务：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now gpu-manager
sudo systemctl status gpu-manager -l
```

查看日志：

```bash
journalctl -u gpu-manager -f
```

普通用户提交作业：

```bash
gpu --socket-path /run/gpu-manager/gpu.sock ls
gpu --socket-path /run/gpu-manager/gpu.sock run --gpus=1 -- python3 train.py
```

daemon 创建的 socket 权限为 `0600`。上面的服务通过 ACL 将 socket 授权给 `gpu-users` 组；如果修改 socket 路径或启动方式，需要同步调整授权。

### 状态和重启行为

启用 `--state-file` 后，调度状态会写入指定文件。daemon 启动时读取该文件，重新接管仍在运行的作业进程，并将已失效的进程回收为失败。状态文件损坏时 daemon 会拒绝启动，以避免静默丢失状态。

daemon 退出不会主动终止作业子进程。`systemctl stop gpu-manager` 后，已运行作业仍可能继续执行，请按运维流程确认后再停止服务。

### 升级

```bash
sudo systemctl stop gpu-manager

sudo install -m 0755 /path/to/gpu-daemon /opt/gpu-manager/gpu-daemon
sudo install -m 0755 /path/to/gpu /opt/gpu-manager/gpu

sudo systemctl start gpu-manager
sudo systemctl status gpu-manager -l
```

升级前建议备份 `/var/lib/gpu-manager/state.json` 和审计日志。

## 安全说明

- gRPC API 只监听 Unix Domain Socket，不开放 TCP 端口；Prometheus 指标仅监听 `127.0.0.1`
- 调用者身份来自 UDS 对端凭证，不能通过环境变量伪造
- `kill` 操作仅允许 root 或作业 owner 执行；普通用户只能查看自己的作业
- daemon 以 root 运行时，作业进程会降到提交者的 UID/GID
- cgroup v2 模式通过设备过滤限制 GPU 设备节点访问，并在终止作业时清理整棵进程树
- 审计日志和状态文件包含用户、作业和进程信息，应限制访问权限

## 故障排查

| 现象 | 检查方向 |
|---|---|
| `无法连接 gpu-daemon` | 确认 daemon 正在运行、socket 路径一致；普通用户确认 ACL 已授权 |
| `NVML 初始化失败` | 检查 NVIDIA 驱动和 `libnvidia-ml.so`；临时验证可使用 `--provider null` |
| `cgroup 前置检查失败` | 检查 `/sys/fs/cgroup/cgroup.controllers` 和写权限；使用 `auto` 或开发环境的 `noop` |
| `/metrics` 端口绑定失败 | 修改 `--metrics-port`，或设为 `0` 关闭 |
| 作业一直排队 | 检查 GPU 是否全部占用、每用户配额、优先级和 `--queue-timeout-secs` |
| `kill` 返回权限错误 | 使用作业 owner 或 root 执行 |
| 状态文件导致启动失败 | 从备份恢复；确认可以丢失该状态后，才可移走文件并重新启动 |

## 代码结构

```text
crates/gpu-core     调度状态机与领域模型
crates/nvmlx        NVML 与 Null Provider 设备后端
crates/gpu-api      gRPC 接口定义与代码生成
crates/gpu-daemon   守护进程、API、作业编排、强制层、审计和指标
crates/gpu-cli      命令行客户端
proto/              gRPC 接口契约
```

完整设计说明见 [`DESIGN.md`](DESIGN.md)。
