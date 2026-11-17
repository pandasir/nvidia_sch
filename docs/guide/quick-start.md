# 快速开始

先用 Null Provider 在没有 NVIDIA GPU 的机器上跑通一次完整调度。下面的流程不需要安装 NVIDIA 驱动。

## 无 GPU 体验

### 1. 准备 socket 目录

```bash
mkdir -p /tmp/gpu-manager
```

### 2. 启动 daemon

在终端 A 运行：

```bash
gpu-daemon \
  --socket-path /tmp/gpu-manager/gpu.sock \
  --provider null \
  --fake-devices 3 \
  --cgroup-mode noop
```

- `--provider null`：使用模拟设备后端
- `--fake-devices 3`：模拟三张 GPU
- `--cgroup-mode noop`：演示环境关闭 cgroup 硬隔离
- `--socket-path`：显式指定 UDS 路径，避免依赖平台默认值

### 3. 查看设备

在终端 B 运行：

```bash
SOCK=/tmp/gpu-manager/gpu.sock

gpu --socket-path "$SOCK" ls
```

此时应能看到三张模拟设备，状态为 `FREE`。

### 4. 提交任务

```bash
gpu --socket-path "$SOCK" run \
  --gpus=1 \
  --description="quickstart" \
  -- sleep 10
```

`gpu run` 会观察任务直到终态。按 Ctrl-C 只退出观察，任务会继续在后台运行。

### 5. 查看作业与队列

```bash
gpu --socket-path "$SOCK" jobs
gpu --socket-path "$SOCK" queue
```

任务结束后，设备重新回到 `FREE`，可以继续提交下一个任务。

> Null Provider 验证的是调度与回收流程，不代表真机 NVML、驱动或 cgroup v2 隔离行为。

## 真实 GPU 环境

真机模式使用 NVML 发现设备，并由 cgroup v2 提供硬隔离。

### 前置条件

- x86-64 Linux，glibc >= 2.34（使用预编译包时）
- 正常安装的 NVIDIA 驱动，且运行时能加载 `libnvidia-ml.so`
- cgroup v2 已挂载且 daemon 有写权限
- 需要硬隔离与进程降权时，以 root 运行 daemon
- 多用户共享 socket 时安装 `acl`，并用 ACL 授权用户组

### 启动 daemon

与无卡演示不同，这里使用 `--provider nvml`，不传 `--fake-devices`，并启用 cgroup v2：

```bash
sudo install -d -m 0755 /run/gpu-manager /var/log/gpu-manager /var/lib/gpu-manager

sudo gpu-daemon \
  --socket-path /run/gpu-manager/gpu.sock \
  --provider nvml \
  --poll-interval-secs 5 \
  --cgroup-mode v2 \
  --audit-log /var/log/gpu-manager/audit.jsonl \
  --state-file /var/lib/gpu-manager/state.json
```

如果希望 cgroup v2 不可用时自动降级，可使用 `--cgroup-mode auto`；`--cgroup-mode noop` 只用于开发或演示，不提供硬隔离。

### 提交任务

```bash
sudo setfacl -m g:gpu-users:rw /run/gpu-manager/gpu.sock

gpu --socket-path /run/gpu-manager/gpu.sock ls
gpu --socket-path /run/gpu-manager/gpu.sock run --gpus=1 -- python3 train.py
gpu --socket-path /run/gpu-manager/gpu.sock jobs
```

生产环境建议使用 systemd 管理 daemon，并完成用户组授权。完整 unit 文件、配额参数和故障排查见 [README 的生产部署章节](https://github.com/pandasir/nvidia_sch#生产部署)。
