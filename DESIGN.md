# GPU 资源调度与分配服务 · 设计文档

> 状态：**设计定稿 v0.2**（5 个开放问题已定案，进入编码前置条件已满足）
> 目标：单机守护进程，调度/分配/回收本机 NVIDIA GPU，供本机用户直接运行进程，以**整卡独占**粒度起步。

---

## 1. 背景与目标

在一台装有 NVIDIA GPU 的机器上，多用户/多任务需要分显卡用。现状往往是"谁先抢到谁用"，缺乏：

- 统一的分配入口与互斥（两个人同时抢同一张卡）
- 配额与公平调度（一个人占满所有卡）
- 强制隔离（拿到卡的人无法偷偷用别人的卡）
- 故障处理与资源自动回收（进程死了卡还挂着）
- 审计与记账（谁在什么时候用了哪张卡、多久）

本服务解决以上问题，核心模型是：

> **资源警察 + 调度器 + 账本**。它在控制面参与（决定"谁能用什么、用多久"），分配完成后完全不插在数据路径上，对被调度进程零额外运行时开销。

## 2. 范围与非目标（V1）

**V1 范围（已确认的约束）：**

| 维度 | 决策 |
|---|---|
| 部署形态 | 单机守护进程（daemon），Unix Socket 本地通信 |
| 使用方式 | 用户提交命令，由服务直接 fork/exec 启动进程 |
| 资源粒度 | 整卡独占（一张卡同一时刻只属于一个租约） |
| 硬件 | NVIDIA 驱动 + NVML 可枚举的 GPU |

**V1 明确不做（但架构留口）：**

- ❌ 多机集群 / 分布式调度（未来：调度核心抽成纯状态机，共识层换 raft/etcd）
- ❌ MIG 硬切分、CUDA MPS 分片共享
- ❌ 容器注入（CDI / Kubernetes device plugin）
- ❌ GPU 间显存 P2P 拓扑感知的精细化放置（v2 候选）
- ❌ 抢占（v1 只分配空闲卡；管理员强制回收属于运维操作，不算抢占）

## 3. 名词与核心概念

| 名词 | 含义 |
|---|---|
| **Device** | 一张物理 GPU，由 daemon 通过 NVML 枚举 |
| **Lease（租约）** | "某张卡在某段时间内属于谁"的授权凭证，带 TTL 与心跳 |
| **Job（任务）** | 用户提交的可执行单元：命令 + 环境 + 资源需求，持有若干 Lease |
| **Scheduler** | 决策核心：把排队请求映射到空闲设备，输出 Lease |
| **Enforcer（强制层）** | 让分配真正生效：环境变量 + device cgroup / eBPF |
| **Credential（凭证）** | Lease 的可序列化表示，用于审计、恢复、吊销 |

## 4. 总体架构

```
┌─────────────┐          ┌──────────────────────────────────────────┐
│  gpu-cli    │          │              gpu-daemon (root)           │
│  (clap CLI) │  gRPC    │  ┌────────────────────────────────────┐  │
│             │──────────▶│  │   API 层 (tonic, Unix Socket)      │  │
└─────────────┘ over UDS │  ├────────────────────────────────────┤  │
                         │  │   调度核心 (纯状态机, 无 I/O)        │  │
                         │  │   · 队列 + 策略                      │  │
                         │  │   · 资源分配 (packing/spread)        │  │
                         │  │   · Lease 生命周期                   │  │
                         │  ├────────────────────────────────────┤  │
                         │  │   强制层 Enforcer                   │  │
                         │  │   · fork/exec (setuid/stdio)        │  │
                         │  │   · CUDA_VISIBLE_DEVICES            │  │
                         │  │   · device cgroup (eBPF 设备过滤)    │  │
                         │  ├────────────────────────────────────┤  │
                         │  │   监控层 Monitor                    │  │
                         │  │   · NVML 轮询 (利用率/显存/温度)     │  │
                         │  │   · Xid 事件订阅 → 设备下线          │  │
                         │  │   · 进程巡查 (资源警察)              │  │
                         │  ├────────────────────────────────────┤  │
                         │  │   存储层 Store                      │  │
                         │  │   · Lease 快照 + 审计事件日志        │  │
                         │  └────────────────────────────────────┘  │
                         └──────────────────────────────────────────┘
```

**分层原则**：调度核心与策略是纯函数/纯状态机，不碰 NVML、不发信号、不写盘。
所有 I/O 都从外层注入。这样策略可以用 mock 设备做单测，且未来可平滑升级为多机。

## 5. 资源模型与设备状态机

V1 中 GPU 是原子单位。每张卡维护状态：

```
       NVML 发现
          │
          ▼
   ┌── Free ◀──────────┐
   │     │ 分配          │ 进程退出/租约过期/管理员释放
   │     ▼              │
   │  Allocated ────────┘
   │     │  Xid 错误/诊断失败
   │     ▼
   └── Offline ──── 人工恢复 ──▶ Free
```

| 设备状态 | 含义 | 处理 |
|---|---|---|
| `Free` | 可分配 | — |
| `Allocated` | 属于某 Lease | 进程 crashe 自动回收；Lease 过期走 grace 期 |
| `Offline` | 故障/维护 | 不参与分配；恢复后重新 Free |

设备信息（NVML 枚举，启动时快照 + 运行期刷新）：
- UUID（**唯一标识，设备引用一律用 UUID**，不使用索引——插槽/拓扑变化会导致索引漂移）、型号、显存总量、PCI 信息
- 运行期：利用率、已用/空闲显存、温度、功耗、时钟、throttle 原因

## 6. 租约模型（Lease）

```
Lease {
  id:          UUID          — 全局唯一
  devices:     [DeviceUUID]  — 占用的物理卡
  owner:       User/Token    — 归属（审计、配额记账）
  job_ref:     JobId         — 关联任务
  status:      Pending / Running / Expiring / Revoked / Released
  issued_at / expires_at:    — 起止时间
  last_heartbeat:            — 心跳续期
}
```

- **发放**：调度器决定分配 → 生成 Lease → 持久化快照 → 返回凭证给调用方。
- **续期**：运行中的 Job 需周期心跳（默认 30s 心跳 / 120s 超时，可配置）。心跳由 daemon 内部维护（进程是 daemon 启动的，可直接由监控循环续期，对用户透明）。
- **过期**：进入 `Expiring` → grace 期（默认 60s，可配置）→ 通知 → 强制回收（杀进程树 + 释放设备）。
- **恢复**：daemon 重启后读快照，逐个校验关联 PID 是否存活且 cgroup 仍存在 → 存活则恢复 `Running`，否则回收。**绝不允许重启后丢失已分配状态。**
- **吊销**：管理员 `KillJob` 直接收回。

> 补充：由于 V1 由 daemon 亲自启动进程，"进程活着"与"租约有效"高度绑定，心跳主要是为了让"过期策略、重启恢复、人工强制回收"有一条统一的记账线索，同时为 v2 的"凭证 + 用户自启进程"模式预留能力。

## 7. 调度器设计

### 7.1 作业生命周期状态机

```
   Submitted ──▶ Queued ──▶ Allocated(Lease 已发) ──▶ Running ──▶ Succeeded
      │            │                                    │           │
      │            │ (无可用设备)                        │           ▼
      └────────────┴──▶ Failed(超时/取消/管理员Kill)  ◀──┴─── Failed
```

- `Queued`：无足够空闲设备时入队。支持用户取消排队（Cancellable）。
  排队超时上限 `max_queue_timeout`（默认 1h，可配置），超时自动取消。
- **取消语义**：用户取消排队中 Job → `Failed(cancelled)`；root/owner 可 Kill 运行中 Job。
- `Allocated`：Lease 发出、进程尚未拉起（或拉起中）。
- `Running`：进程已 exec，等待退出。
- 终态：`Succeeded`（退出码 0）/ `Failed`（非零、被 Kill、超时、设备故障）。

### 7.2 队列与调度策略

- 队列按 `(优先级, 到达时间)` 排序；同优先级 FIFO。
- **分配算法（v1）**：请求 N 张卡 → 在空闲设备中选 N 张。两种放置策略可配置：
  - `packing`（默认）：优先填满碎片化较小的卡，减少整体碎片，提高利用率。
  - `spread`：分散到不同卡，降低单卡故障波及面。
  > ⚠️ 实现现状：仅 `packing` 落地（按设备 UUID 稳定顺序取前 N 张空闲卡）。`spread` 未实现。
- **公平份额**：按用户配额记账，防止单用户占满整机；超配额请求入队等待或拒绝（可配置）。
- **无抢占**：v1 不抢占运行中任务。管理员 `KillJob` 是显式运维操作。

### 7.3 配额（v1 简版）

- 全局配置：`max_gpus_per_user`（默认不限）/ `max_concurrent_jobs_per_user`。
- 记账数据来自事件日志，天然支持将来做 chargeback。

## 8. 进程启动与强制层（Enforcer）

这是 V1 工程量的重头，也是"分配 ≠ 生效"的关键一环。

### 8.1 启动流程

1. 调度器产出 Lease → 进入 `Running`。
2. Enforcer 构造进程：
   - `setuid/setgid` 降到请求者身份（daemon 以 root 运行，子进程必须降权）。
   - 转接 stdin/stdout/stderr：**支持 pty 做交互式终端直通**（`gpu run` 交互可用），也支持重定向落盘日志。
   - 注入 `CUDA_VISIBLE_DEVICES=<分配卡的UUID列表>`（软约束，防呆/常规路径生效）。
   - 工作目录、env、argv 透传。
3. **硬约束**：把进程放入专用 cgroup `gpu-lease-<id>`，该 cgroup 挂 eBPF 设备过滤，**只允许访问已分配设备的 major:minor**。用户改环境变量无法突破 cgroup。
4. 启动成功后，监控循环接管进程存活与心跳。

### 8.2 资源回收

- 进程退出 → daemon reap → 释放 cgroup → Lease 置 `Released` → 设备回 `Free`。
- **杀整棵进程树**：用 cgroup 的 `cgroup.kill`（v2）而非只 kill 父进程，防止孙进程逃逸继续占用。
- 子进程 fork 的孙进程天然继承 cgroup，无需额外处理。

### 8.3 防泄漏注意事项（编码时落实）

- 子进程全部 fd 设 `O_CLOEXEC`，绝不把 daemon 的 UDS/gRPC 连接泄漏给子进程。
- cgroup 控制器路径与权限：需要挂载 cgroup v2，daemon 启动时做前置检查，失败则明确报错退出（fail-fast）。
- 进程组与会话：`setsid`（如非交互）便于统一信号管理。

## 9. 监控与故障处理

### 9.1 监控循环（独立线程，NVML 是同步 FFI）

- 周期（默认 5s）轮询每设备：利用率、显存、温度、功耗、throttle 原因。
- 指标暴露为 Prometheus 文本格式 `/metrics`（axum 或手写 tiny http）。
- **进程级用量**：NVML running-process 接口（每进程显存）；可选 GPM 拿真实算力占用。

### 9.2 资源警察（进程巡查）

每个轮询周期比对："已分配卡上当前 NVML 报告的所有 CUDA 进程 ⊆ 该卡 Lease 关联进程集合"。
- 发现陌生进程 → 日志告警；可配置处置：仅告警 / 通知管理员 / 直接 kill（默认仅告警，v1 保守）。
- 这是整卡独占模式下成本最低却最有效的治理手段。

### 9.3 故障处理

| 事件 | 处置 |
|---|---|
| Xid 错误（NVML 事件订阅） | 设备置 `Offline` → 回收受影响 Lease → 通知 owner |
| 显存/温度/功耗异常阈值 | 告警；温度超限由驱动自行降频，不做主动动作（v1） |
| 设备消失（热插拔/驱动重载） | 同 Xid 处理 |
| 进程意外死亡 | 立即回收，Lease 走 `Failed`（退出码非零） |

### 9.4 判定注意

- 进程启动初期显存从 0 爬坡、利用率有尖峰，**一切基于瞬时值的判定都要加观测窗口**，避免误杀。
- 故障判定连续 N 个周期才触发下线，防抖。

## 10. 存储与恢复

- 双轨：
  - **Lease 快照**：每次状态变更写一份（`redb` 或 `rusqlite`，单文件嵌入式，daemon 启动时加载。
    > M4 落地为 **JSON + 原子 rename**(`--state-file`，tmp 写 + rename，含 scheduler 快照/进程 pid/owner 记账)。
    > 轻依赖的务实偏差：单机状态量小(无 ACID 并发需求)，JSON 可读可审计；文件损坏拒绝启动而非静默丢状态。
  - **审计事件日志**：append-only 文本（JSON Lines），`{ts, event, user, lease, devices, detail}`，用于审计/记账/复盘。
- 恢复流程（见 §6）：先加载快照，再逐个校验进程存活，得到"恢复 vs 回收"判定，写恢复事件。
- 事件日志即"事实来源"（source of truth），快照是加速恢复的缓存——两者顺序写入要保证一致性（先落日志，再更新快照）。

## 11. API 设计（gRPC，Unix Socket）

- 传输：gRPC over UDS（文件权限 0600 root / 或配置用户组），不监听 TCP（本地服务，无网络面）。
- 方法（v1 全集，宁少勿多）：

| 方法 | 说明 |
|---|---|
| `ListDevices` | 设备当前状态列表 |
| `WatchDevices` | 流式推送设备状态变更 |
| `SubmitJob` | 提交任务：卡数、优先级、命令、env、工作目录 → `JobId`（可能直接 Running 或 Queued） |
| `GetJob` / `ListJobs` | 任务详情 / 列表 |
| `CancelJob`（排队中） | 取消排队 |
| `KillJob`（管理员） | 强制终止运行中任务（杀进程树 + 回收） |
| `RenewLease` | 预留（v1 由 daemon 内部心跳） |
| `GetMetrics` | 拉取当前设备/进程快照（Prometheus 从 /metrics 拉） |

- CLI（`gpu` 子命令）：`gpu ls` / `gpu watch` / `gpu run --gpus=N CMD…` / `gpu jobs` / `gpu kill <job>` / `gpu queue`。
- 可选 Web 面板 v2 再议。

## 12. 模块划分（Cargo workspace）

> 现状:仓库已按本结构落地为 Cargo workspace(`gpu-api` 含 proto 代码生成,
> `gpu-core` 为纯状态机,`nvmlx` 设备抽象,`gpu-daemon`/`gpu-cli` 为可执行入口)。

```
gpu-manager/                 (workspace)
├── Cargo.toml
├── proto/                   gRPC 定义 (.proto → 生成代码)
├── crates/
│   ├── gpu-core/            纯状态机：设备/Lease/Job、队列、放置策略、配额
│   │                        · 零 I/O，trait 化外围依赖 → mock 单测
│   ├── nvmlx/               NVML 抽象 trait + nvml-wrapper 实现
│   │                        · Device 查询、事件订阅、running-process、进程优先级控制
│   ├── gpu-daemon/          编排：gRPC 服务、Enforcer(进程/cgroup)、Monitor、Store、恢复
│   └── gpu-cli/             clap CLI 客户端
```

- `gpu-core` 是唯一业务逻辑所在，其余都是"外设"。
- 依赖清单（草案）：tokio、tonic/prost、nvml-wrapper、redb 或 rusqlite、clap、serde、tracing、nix、axum（仅 /metrics）、anyhow/thiserror。
- 测试策略：`gpu-core` 用 mock 设备做全量状态机单测；`nvmlx` 对真实驱动做集成冒烟测试（无卡环境跳过）；`gpu-daemon` 用假 Enforcer 做端到端测试。

## 13. 安全与多租户

- 传输：仅 UDS，权限 0600；daemon root，CLI 按真实身份（SO_PEERCRED）识别调用者。
- 授权：**KillJob 仅允许 root 与 job owner**；用户只能查看/操作自己的 Job（root 全量），排队取消同理。
- 隔离：device cgroup 是硬墙，`CUDA_VISIBLE_DEVICES` 是软扶手。
- 降权：子进程 setuid/setgid 到请求者，杜绝 root 逃逸面。
- 审计：所有分配/释放/失败/Kill 事件落日志，含身份与设备。
- 配额：见 §7.3。

## 14. 已知风险与对策

| 风险 | 对策 |
|---|---|
| NVML 是同步 FFI，阻塞 async 执行器 | 监控/查询全部放专用线程或 `spawn_blocking` |
| cgroup v2 设备过滤实现复杂（eBPF） | 启动前置检查 fail-fast；用 `cgroup.kill` 收进程树 |
| 进程显存统计不准（idle context 占显存） | 以 NVML running-process 为准 + 观察窗口；进程巡查只做告警 |
| daemon root + 子进程降权边界出错 | setuid 后立即 discard 特权路径；子进程不继承 daemon fd |
| 重启恢复丢状态 | 先日志后快照；恢复时校验 PID/cgroup 存活 |
| 用户绕过 env 直接用别的卡 | cgroup 硬墙兜底（§8.1） |
| 显存被占满导致其他租约 OOM（整卡独占下不存在跨租约共享，风险低） | v1 整卡独占天然隔离；v2 分片再引入显存配额 |

## 15. 里程碑（MVP 路线图）

| 里程碑 | 内容 | 验收标准 | 状态 |
|---|---|---|---|
| **M0 骨架** | workspace + proto + crate 划分 + CI | 空壳可构建、单测可跑 | ✅ 2026-08-25 完成 |
| **M1 设备发现与监控** | NVML 枚举、轮询、/metrics | `gpu ls` 显示真实设备与实时状态 | ✅ 2026-08-25 完成 |

> M1 备注:为让 `gpu ls` 端到端可用,gRPC 传输层(proto 代码生成 + tonic over UDS)
> 提前到 M1 落地,当前仅实现只读 RPC(ListDevices/WatchDevices/GetMetrics);
> 作业类 RPC 在 M2 接入。protoc 解析优先 vendored 预编译二进制,免安装 protoc。
| **M2 整卡分配闭环** | 状态机、队列、SubmitJob→启动→退出回收 | 两进程抢 1 张卡：一个跑完另一个才跑；kill 后卡立即释放 | ✅ 2026-08-26 完成 |

> M2 备注：作业类 RPC(SubmitJob/GetJob/ListJobs/CancelJob/KillJob)与 CLI
> (`gpu run`/`jobs`/`queue`/`kill`)已接入。M2 的"强制"仅含软约束
> (`CUDA_VISIBLE_DEVICES` 注入)；device cgroup 硬墙、setuid 降权、pty、
> 审计日志与 Kill 权限(root/SO_PEERCRED)属 M3。M2 按 daemon 运行用户
> 记账 owner,身份识别待 M3 接入。daemon 退出不杀子进程,为 M4 重启恢复预留。
| **M3 强制与安全** | device cgroup、setuid、pty 交互直通、进程树回收、审计日志、Kill 权限 | 无卡测试环境用手写假 cgroup 验证；真机验证绕不过 cgroup | ✅ 2026-08-26 完成 |

> M3 备注：身份/权限——owner 改自 UDS 对端凭证(SO_PEERCRED/getpeereid,
> tonic `UdsConnectInfo.peer_cred` 内置)，客户端不可伪造；Kill/Cancel 仅
> root 与 job owner(ADR-1)；普通用户 jobs 只见自己的作业。
> 强制层——device cgroup v2：手写 eBPF(`BPF_PROG_TYPE_CGROUP_DEVICE`)
> 按 (major,minor) 白名单放行(含 nvidiactl/nvidia-uvm/基础设备)，`cgroup.kill`
> 杀整棵进程树；`--cgroup-mode v2` 探测失败启动即退(fail-fast)，`auto`
> 降级 noop 仅告警；daemon 为 root 时子进程 setuid/setgid 降到请求者。
> 审计——JSONL append-only(`--audit-log`)，记录提交/排队/分配/运行/结束/
> Kill/失败及 user/uid/设备。pty 交互直通未随 M3 落地(proto 无 I/O 流)，延后。
> 验收：无卡容器(amd64)实测 alice 作业子进程 uid=1000、alice kill root
> 作业被拒、v2 模式下无 cgroup 直接失败；真机 cgroup 硬墙仍需真机复核。
| **M4 故障与恢复** | Xid 订阅、设备下线、重启恢复 | 杀进程 → 自动回收；重启 daemon → 状态正确恢复 | ✅ 2026-08-27 完成 |
> M4 已落地：**monitor 设备清单对账**(列表消失连续 2 轮 → 下线，防抖；新卡热插拔注册；消失卡复现 → 恢复上线)；
> **faults**：设备下线撤销租约 + 杀整棵进程树(cgroup.kill + SIGKILL) + cgroup 清理，双信号(Xid 事件 / 清单消失)统一入口；
> **Xid 订阅**(仅 Linux，nvml-wrapper EventSet，CRITICAL_XID_ERROR → 立即下线)；
> **重启恢复**(`--state-file`)：恢复进程校验 pid 存活(Linux 排除僵尸)→ 恢复 Running 或按失败回收，
> 恢复的孤儿进程退出码不可考(非其父进程,无法 wait)按"状态未知"回收为 Failed；daemon 退出不杀子进程。
> 验收：amd64 容器实测提交 → 杀 daemon → 重启 → 作业恢复 Running → 自然结束自动回收 → 设备 Free → 新作业可运行；
> 外部 SIGKILL 子进程 → reaper 自动回收并推进队列(单测/单元覆盖)；Xid 链路需真机复核(无卡环境不激活)。
| **M5 公平与配额** | 多用户配额、公平份额、排队策略调优 | 配额测试用例通过 | ✅ 2026-08-27 完成（v1 仅配额） |
> M5 已落地(§7.3 v1 简版，`--max-gpus-per-user` / `--max-concurrent-jobs-per-user`，默认 0=不限)：
> **在飞卡数配额**：超配额新请求**入队等待**；配额门禁先于资源检查，且只约束该用户自身 ——
> 配额受限作业被跳过、不阻塞其他用户(与"资源不足整队阻塞"的 M2 队首阻塞语义区分)；
> **并发作业配额**：非终态作业数超限，提交直接拒绝(gRPC `RESOURCE_EXHAUSTED`)；
> 记账纯内存遍历(leases/jobs)，随调度快照持久化，重启后配额继续生效；
> 旧状态文件缺配额字段 → `#[serde(default)]` 补 0(不限)，向后兼容。
> 验收：amd64 容器实测同用户第 2 个作业排队 → kill 后自动运行完成；并发超限提交被拒(含配额提示)；63 测试全绿。
> **v1 明确未落地**：**公平份额**(加权记账 / aging 防饿死)与**排队策略调优**均未实现，
> 排队仍为「严格优先级 + 队首阻塞」；§7.2 的 `spread` 放置策略代码中不存在(仅 `packing`)。

## 16. 设计决策记录（ADR）

| # | 问题 | 决策 | 落点 |
|---|---|---|---|
| 1 | Kill/取消权限模型 | **仅 root 与 job owner**；用户只能操作自己的 Job | §13、§7.1 |
| 2 | 排队时长上限 | **默认 1h，超时自动取消**（`max_queue_timeout` 可配置） | §7.1 |
| 3 | 交互式任务 | **第一版支持**：pty 终端直通 | §8.1 |
| 4 | 设备标识 | **一律以 UUID 为准**，不用索引 | §5 |
| 5 | 常驻方式 | **systemd unit**（`gpu-daemon.service`，daemon 启动前置检查 cgroup v2） | — |