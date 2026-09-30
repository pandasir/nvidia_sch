# GPU Manager (Rust)

单机 NVIDIA GPU 调度与分配服务:调度/分配/回收本机显卡资源,供本机用户直接运行进程,以**整卡独占**粒度起步。

> 完整设计文档见 [`DESIGN.md`](DESIGN.md):架构分层、租约模型、强制层(device cgroup)、监控故障、里程碑与 ADR 均已定稿。

## 当前进度:M5 公平与配额 ✅

- **gpu-core**(纯状态机,零 I/O):优先级队列、队首阻塞放置、租约 TTL + 心跳、排队超时、设备下线/恢复、Kill/取消、快照恢复、**M5 配额(在飞卡数 / 并发作业数)** — 22 个单测
- **gpu-api**:`proto/gpu.proto` → tonic/prost 代码生成(vendored protoc,免安装)
- **gpu-daemon**:
  - **enforcer**:进程启动(注入 `CUDA_VISIBLE_DEVICES` 软约束 + env/workdir 透传 + **setuid/setgid 降权**;pty 交互在 M3 之后)
  - **jobs**:SubmitJob → 进程拉起 → 退出回收 闭环;reaper 周期回收已退出进程并推进队列;外部 SIGKILL → **自动回收**(Failed + 释放设备 + 队首推进)
  - **identity**:调用者身份取自 UDS 对端凭证(**SO_PEERCRED**/getpeereid),不可伪造;KillJob/CancelJob 仅 **root 与 job owner**(ADR-1),普通用户只见自己的作业
  - **cgroup**:device cgroup 硬墙 — 手写 eBPF(`BPF_PROG_TYPE_CGROUP_DEVICE`)按 (major,minor) 白名单放行、`cgroup.kill` 杀整棵进程树;`auto/v2/noop` 三模式,v2 探测失败 **fail-fast**;无卡环境用 FakeCgroup 验证生命周期
  - **audit**:JSONL 审计日志(提交/排队/分配/运行/结束/Kill/失败/设备下线/恢复,含 user/uid/设备)
  - **M4 — 故障与恢复**:
    - **monitor 设备清单对账**:列表消失连续 2 轮(防抖)→ 设备下线;新卡热插拔注册;消失卡复现 → 恢复上线
    - **faults**:设备下线撤销租约 + 杀进程树(cgroup.kill + SIGKILL)+ cgroup 清理;双故障信号统一入口
    - **Xid 订阅**(仅 Linux):NVML `CRITICAL_XID_ERROR` 事件 → 立即触发设备下线(真机复核项)
    - **重启恢复**(`--state-file`):JSON 状态文件(原子写)每次状态变更落盘;重启后校验进程 pid 存活(Linux 排除僵尸)→ 恢复 Running 或按失败回收;**daemon 退出不杀子进程**
  - **M5 — 公平与配额**(§7.3):
    - `--max-gpus-per-user N`:每用户同时在飞(Allocated+Running)最大卡数;超配额的**新请求入队等待**,且**不阻塞其他用户**(配额门禁先于资源检查,只约束该用户自身)
    - `--max-concurrent-jobs-per-user N`:非终态(排队+运行)作业数上限,超限提交直接拒绝(gRPC `RESOURCE_EXHAUSTED`)
    - 记账纯内存(leases/jobs 遍历),随调度快照持久化,**重启后配额继续生效**;旧状态文件缺配额字段自动按不限补齐
- **gpu-cli**:`gpu run`(阻塞观察到终态)/ `gpu jobs` / `gpu queue` / `gpu kill` 可用
- 17 个端到端集成测试:真实 UDS gRPC + NullProvider 模拟设备,覆盖提交运行/两进程抢 1 卡排队/杀进程释放/取消排队/命令不存在/审计轨迹/Killer 审计/重启恢复(Running 恢复、死进程回收、恢复后新作业可运行)/**配额(超配额排队→kill 后自动运行、并发超限拒绝、配额重启后生效**)
- 真机行为验证(cgroup 硬墙无法在无卡环境验证):amd64 容器冒烟 — 非 root 用户(alice)提交的作业子进程真实降权为 uid=1000、alice 看不到 root 的作业、alice kill root 作业被拒(PermissionDenied)、容器内 `--cgroup-mode v2` 启动即失败;M4 冒烟(提交→杀 daemon→重启→恢复 Running→自动回收→设备 Free);**M5 冒烟(同用户第 2 个作业排队、kill 后自动运行、并发超限被拒)**

> 无卡环境可直接 `cargo test --workspace` 验证(63 个测试,含 macOS;Xid 链路为 Linux-only,真机复核)。
> 恢复的孤儿进程退出码不可考(daemon 重启后非其父进程),统一按"状态未知"回收为 Failed,资源正常释放。

## 构建与测试(无需 GPU)

```bash
cargo build --workspace
cargo test  --workspace
```

## 交叉编译 Linux amd64

在 macOS(Apple Silicon)上交叉编译 `x86_64-unknown-linux-gnu` release 二进制
(glibc 动态链接,兼容主流发行版)。需要:rustup 目标 + zig 0.13(自带 glibc 交叉链接):

```bash
rustup target add x86_64-unknown-linux-gnu
# zig 0.13 下载:https://ziglang.org/download/(或 brew install zig 后对应调整路径)

export ZIG_GLOBAL_CACHE_DIR="$PWD/.tools/zig-cache/global"
export ZIG_LOCAL_CACHE_DIR="$PWD/.tools/zig-cache/local"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/.tools/zig-macos-aarch64-0.13.0/zig"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-target -C link-arg=x86_64-linux-gnu"
cargo build --release --target x86_64-unknown-linux-gnu --workspace --bins
```

产物:`target/x86_64-unknown-linux-gnu/release/{gpu-daemon,gpu}`。
现成产物见 [`dist/linux-amd64/`](dist/linux-amd64/)(含 systemd 部署建议与校验和)。
> 选择 gnu(glibc)而非 musl:真机场景 daemon 需在运行时 dlopen glibc 链接的
> `libnvidia-ml.so`,musl 静态进程无法加载。

## 快速上手(无 GPU 机器演示)

```bash
# 终端 1:启动 daemon(3 台模拟设备;建议同时开启审计与状态文件以演示重启恢复)
# M5 配额:--max-gpus-per-user 2 每人最多同时占 2 卡;--max-concurrent-jobs-per-user 3 每人最多 3 个作业
cargo run -p gpu-daemon -- --provider null --fake-devices 3 --poll-interval-secs 2 \
     --audit-log /tmp/gpu-audit.jsonl --cgroup-mode noop --state-file /tmp/gpu-state.json \
     --max-gpus-per-user 2 --max-concurrent-jobs-per-user 3

# 终端 2:查看设备实时状态
cargo run -p gpu-cli -- ls

# 实时刷新(监控广播)
cargo run -p gpu-cli -- watch

# 提交并运行任务(需要 2 张卡;资源不足自动排队)
cargo run -p gpu-cli -- run --gpus=2 -- sleep 30

# 另开终端:任务列表 / 排队情况 / 强制终止(杀进程 + 立即释放卡;仅 root 与 job owner 可 Kill)
cargo run -p gpu-cli -- jobs
cargo run -p gpu-cli -- queue
cargo run -p gpu-cli -- kill <job_id>

# 审计日志(JSONL,每行一个事件)
cat /tmp/gpu-audit.jsonl

# Prometheus 指标
curl -s http://127.0.0.1:9900/metrics
```

## 目录

```
crates/gpu-core     调度核心:领域模型 + 纯状态机(零 I/O,全量单测)
crates/nvmlx        NVML 访问抽象:DeviceProvider trait + NullProvider(mock) + NvmlProvider(真机)
crates/gpu-api      proto/gpu.proto → tonic/prost 代码生成(vendored protoc,免安装)
crates/gpu-daemon   守护进程:api(gRPC) + state/monitor(状态与轮询)
                    + enforcer/cgroup(进程启动与强制) + identity/audit(身份与审计)
                    + jobs/faults/recovery/xid(作业编排与故障恢复) + metrics
crates/gpu-cli      CLI 客户端:ls/watch/run/jobs/queue/kill
proto/              V1 gRPC 接口契约
```

## 里程碑

| 里程碑 | 状态 |
|---|---|
| M0 骨架(workspace + proto + crate 划分) | ✅ 2026-08-25 |
| M1 设备发现与监控(NVML 枚举、轮询、/metrics) | ✅ 2026-08-25 |
| M2 整卡分配闭环(SubmitJob → 启动 → 退出回收) | ✅ 2026-08-26 |
| M3 强制与安全(device cgroup、setuid、审计、Kill 权限;pty 延后) | ✅ 2026-08-26 |
| M4 故障与恢复(Xid 订阅、设备下线、重启恢复) | ✅ 2026-08-27 |
| M5 公平与配额(**v1 仅配额**:在飞卡数 / 并发作业数) | ✅ 2026-08-27 |

> **v1 未落地**:公平份额(加权记账 / aging 防饿死)、排队策略调优、`spread` 放置策略(当前仅 `packing`)、pty 交互直通;
> device cgroup 硬墙与 Xid 链路需真机复核。详见 [DESIGN.md](DESIGN.md) §7.2 / §15。
