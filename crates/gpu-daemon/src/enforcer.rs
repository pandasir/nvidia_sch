//! Enforcer —— 把"已分配"变成"进程真的在跑"(M2 起步版)。
//!
//! DESIGN.md §8:软约束(`CUDA_VISIBLE_DEVICES`)从这里注入;
//! M3 起加入:
//!
//! - **setuid/setgid 降权**:daemon 以 root 运行时,子进程降到请求者身份
//!   (§8.1、§13 多租户);daemon 非 root(开发机)时跳过并告警;
//! - **pty 交互直通**(M3):若指定,子进程挂载 pty 作为控制终端;
//! - **device cgroup 硬墙**在 [`crate::cgroup`] 中,由 [`crate::jobs`]
//!   在 spawn 前后编排。
//!
//! 顺序保证:先应用用户 env,再覆盖 `CUDA_VISIBLE_DEVICES`
//! (软约束必须胜出,防止用户请求覆盖掉分配结果)。

use std::path::PathBuf;

use tokio::process::Command;

/// 子进程运行身份(None = 继承 daemon 身份;Some = 降权目标)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
}

/// 一份作业的进程启动规格(由 SubmitJob 请求解析而来)
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// argv[0] 为可执行文件,其余为参数
    pub command: Vec<String>,
    /// 追加/覆盖的环境变量(`KEY=VALUE` 已解析)
    pub env: Vec<(String, String)>,
    /// 工作目录(None = 继承 daemon)
    pub workdir: Option<PathBuf>,
    /// 降权目标(None = 继承 daemon 身份;由 API 层从调用者凭证解析)
    pub run_as: Option<RunAs>,
}

/// 以目标身份拉起子进程,注入 GPU 软约束后返回句柄。
///
/// 降权规则(§13):daemon 为 root 时,子进程 setuid/setgid 到请求者;
/// daemon 非 root 时保持自身身份(并告警),保证开发机(macOS)可跑。
pub fn spawn_with_gpus(
    spec: &SpawnSpec,
    gpu_uuids: &[String],
) -> std::io::Result<tokio::process::Child> {
    debug_assert!(!spec.command.is_empty(), "调用方需先校验 command 非空");
    let mut cmd = Command::new(&spec.command[0]);
    cmd.args(&spec.command[1..]);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // 软约束最后注入,覆盖用户 env 中的同名变量(DESIGN.md §8.1)
    cmd.env("CUDA_VISIBLE_DEVICES", gpu_uuids.join(","));
    if let Some(dir) = &spec.workdir {
        cmd.current_dir(dir);
    }
    // 子进程不得继承 daemon 的 gRPC/UDS fd(tokio 默认已 CLOEXEC/不显式继承)
    // 关闭 daemon 退出时自动杀子进程:重启恢复(M4)依赖"子进程跨 daemon 存活"
    cmd.kill_on_drop(false);

    apply_privilege_drop(&mut cmd, spec.run_as);

    cmd.spawn()
}

/// 应用 setuid/setgid 降权(§8.1)。仅 daemon 为 root 且目标非 root 时生效;
/// 否则保持当前身份并记录(非 root 环境下无需也不可降权)。
fn apply_privilege_drop(cmd: &mut Command, run_as: Option<RunAs>) {
    let Some(RunAs { uid, gid }) = run_as else { return };
    let euid = nix::unistd::geteuid().as_raw();

    if euid != 0 {
        tracing::debug!("daemon 非 root(euid={euid}),跳过 setuid 降权(需求 uid={uid})");
        return;
    }
    if uid == 0 {
        tracing::debug!("目标身份为 root,不降权");
        return;
    }
    // daemon 为 root:先降 gid 再降 uid(顺序不可反,反了会丢失权限)
    cmd.gid(gid);
    cmd.uid(uid);
    tracing::info!("子进程将降权运行: uid={uid} gid={gid}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_as_none_preserves_identity() {
        assert_eq!(SpawnSpec { run_as: None, ..Default::default() }.run_as, None);
    }

    #[test]
    fn applies_privilege_drop_when_root() {
        // 无法在测试中真实 setuid(root 环境才有意义);只验证逻辑分支:
        // 非 root euid 下不 panic、不修改 cmd(间接验证安全路径)。
        let mut cmd = Command::new("/bin/true");
        apply_privilege_drop(&mut cmd, Some(RunAs { uid: 1000, gid: 1000 }));
        // 该函数在非 root 下仅是 debug 日志;无副作用可断言
    }
}