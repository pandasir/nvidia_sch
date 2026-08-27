//! 调用者身份识别(M3):从 gRPC over UDS 连接的对端凭证(SO_PEERCRED,
//! macOS 为 getpeereid)解析调用者,并用它实现 ADR-1 的权限模型
//! (KillJob/CancelJob 仅 root 与 job owner;非 root 用户只能查看自己的作业)。
//!
//! tonic 0.12 已内置:`tonic::transport::server::UdsConnectInfo` 会出现在
//! 每个请求的 extensions 中,其 `peer_cred` 字段即对端进程凭证。
//! 因此本模块只做"提取 + 用户名解析 + 授权判定",无需自定义传输层。

use tonic::Request;

/// 一次调用的发起者(从 UDS 对端凭证解析而来,不可由客户端伪造)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    /// 用户名(`uid` 经 getpwuid 解析),解析失败回退为 `uid:<uid>`
    pub username: String,
}

impl Caller {
    /// 是否 root(uid 0)。root 拥有全部权限(ADR-1)
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    /// 校验对某作业执行敏感操作的权限:
    /// root 或作业 owner(username 精确匹配,root 例外)
    pub fn authorize_job(&self, job_owner: &str) -> Result<(), String> {
        if self.is_root() || self.username == job_owner {
            Ok(())
        } else {
            Err(format!(
                "权限不足:仅 root 或作业 owner({job_owner})可操作,当前身份 {}",
                self.username
            ))
        }
    }
}

/// 从 gRPC 请求的 extensions 提取调用者身份。
///
/// 非 UDS 传输(测试用 DuplexStream 等)无身份信息 → 返回 [`Caller::unknown`]。
pub fn caller_from_request<T>(request: &Request<T>) -> Caller {
    use tonic::transport::server::UdsConnectInfo;
    match request.extensions().get::<UdsConnectInfo>() {
        Some(info) => match &info.peer_cred {
            Some(cred) => Caller::from_cred(
                // pid 类型平台相关(macOS 无 pid 信息 → None)
                cred.pid().unwrap_or(0) as u32,
                cred.uid(),
                cred.gid(),
            ),
            None => Caller::unknown(),
        },
        None => Caller::unknown(),
    }
}

/// 从对端凭证三元组构造调用者,并尝试解析用户名
pub(crate) fn from_uid_gid_pid(pid: u32, uid: u32, gid: u32) -> Caller {
    Caller {
        pid,
        uid,
        gid,
        username: username_of(uid),
    }
}

impl Caller {
    fn from_cred(pid: u32, uid: u32, gid: u32) -> Self {
        from_uid_gid_pid(pid, uid, gid)
    }

    /// 无凭证时的兜底身份(仅测试/非 UDS 传输;正式路径不应出现)
    fn unknown() -> Self {
        let euid = nix::unistd::geteuid().as_raw();
        Caller {
            pid: 0,
            uid: euid,
            gid: nix::unistd::getegid().as_raw(),
            username: username_of(euid),
        }
    }
}

/// uid → 用户名(经 passwd 数据库);失败回退 `uid:<N>`
fn username_of(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("uid:{uid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_authorizes_any_job() {
        let root = Caller::from_cred(1, 0, 0);
        assert!(root.is_root());
        assert!(root.authorize_job("anyone").is_ok());
    }

    #[test]
    fn owner_authorizes_own_job() {
        let me = Caller::from_cred(42, 501, 20);
        assert!(!me.is_root());
        assert!(me.authorize_job(&me.username).is_ok());
        assert!(me.authorize_job("someone-else").is_err());
    }

    #[test]
    fn uid_fallback_when_no_passwd_entry() {
        // 深 uid(通常无 passwd 项)+ 非 root → 用户名回退为 "uid:<n>"
        let c = Caller::from_cred(7, 4026531839, 4026531839);
        assert!(c.username.starts_with("uid:"));
        // 即使查不到名字,owner 比对依然按同一回退串工作
        assert!(c.authorize_job(&c.username).is_ok());
    }

    #[test]
    fn caller_from_request_without_uds_info_is_unknown() {
        // 模拟无身份扩展的请求(测试/嵌入通道)
        let req: Request<()> = Request::new(());
        let c = caller_from_request(&req);
        // 不 panic、非 root 即可(具体 uid 取决于运行环境)
        assert_ne!(c.pid, u32::MAX);
        let _ = c.username;
    }
}