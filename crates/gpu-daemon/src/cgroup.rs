//! device cgroup(M3 强制层硬墙):把子进程放入专用 cgroup,用 eBPF
//! 设备过滤限制其只能访问已分配 GPU 的设备节点(DESIGN.md §8.1)。
//!
//! - DeviceCgroup:统一抽象(创建/加入/杀树/清理);
//! - NoopCgroup:无 cgroup 环境(macOS 开发机、容器无权限)的安全降级——
//!   不隔离但**不报错**,仅依赖软约束 `CUDA_VISIBLE_DEVICES`;
//! - V2Cgroup:cgroup v2 真机实现(Linux)。设备过滤 = 手写 eBPF
//!   (`BPF_PROG_TYPE_CGROUP_DEVICE`)按 (major,minor) 白名单;
//!   进程树回收 = `cgroup.kill`(杀整棵树,防孙进程逃逸)。
//!
//! daemon 启动时按 `CgroupMode` 探测:
//! - `auto`:探测成功用 V2,否则 Noop + 告警;
//! - `v2`:探测失败**直接退出**(fail-fast,DESIGN.md §14);
//! - `noop`:强制不隔离(测试/开发)。

use crate::DeviceSnapshot;
use std::path::Path;

/// 设备访问白名单条目:(major, minor, access 掩码)
/// access:1=read, 2=write, 4=mknod;GPU 需要全部(7)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevSpec {
    pub major: u32,
    pub minor: u32,
    pub access: u8,
}

impl DevSpec {
    pub const RW_MKNOD: u8 = 0b111;

    pub const fn new(major: u32, minor: u32) -> Self {
        Self {
            major,
            minor,
            access: Self::RW_MKNOD,
        }
    }
}

/// cgroup 操作错误(携带人类可读原因)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cgroup 错误: {0}")]
pub struct CgroupError(pub String);

impl From<std::io::Error> for CgroupError {
    fn from(e: std::io::Error) -> Self {
        CgroupError(e.to_string())
    }
}

/// 隔离抽象:把"已分配"变成"硬性只能访问自己的卡"
pub trait DeviceCgroup: Send + Sync {
    /// 为租约创建隔离 cgroup(含设备过滤白名单)
    fn create(&self, lease_id: &str, devices: &[DevSpec]) -> Result<(), CgroupError>;
    /// 把进程放入该 cgroup
    fn add_pid(&self, lease_id: &str, pid: u32) -> Result<(), CgroupError>;
    /// 杀 cgroup 内整棵进程树(含孙进程)
    fn kill_all(&self, lease_id: &str) -> Result<(), CgroupError>;
    /// 拆除(detach 过滤程序 + 删除目录)。失败仅告警(由调用方容忍)
    fn destroy(&self, lease_id: &str) -> Result<(), CgroupError>;
}

// ---------------------------------------------------------------------------
// Noop:无隔离(开发环境)
// ---------------------------------------------------------------------------

/// 空实现:不隔离、不报错(配合软约束使用)。`enabled()` 恒为 false。
pub struct NoopCgroup;

impl DeviceCgroup for NoopCgroup {
    fn create(&self, _lease_id: &str, _devices: &[DevSpec]) -> Result<(), CgroupError> {
        Ok(())
    }
    fn add_pid(&self, _lease_id: &str, _pid: u32) -> Result<(), CgroupError> {
        Ok(())
    }
    fn kill_all(&self, _lease_id: &str) -> Result<(), CgroupError> {
        Ok(())
    }
    fn destroy(&self, _lease_id: &str) -> Result<(), CgroupError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// cgroup v2 真机实现(Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod v2 {
    use super::*;
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::Mutex;

    pub struct V2Cgroup {
        root: PathBuf,
        /// lease_id -> 已 attach 的 eBPF 程序 fd(destroy 时 detach + close)
        progs: Mutex<HashMap<String, i32>>,
    }

    impl V2Cgroup {
        /// 探测 cgroup v2:已挂载 + 可创建子目录 → Ok;否则 Err(原因)
        pub fn probe(root: &Path) -> Result<Self, String> {
            let controllers = root.join("cgroup.controllers");
            if !controllers.is_file() {
                return Err(format!(
                    "{} 不存在:请确认 cgroup v2 已挂载(mount | grep cgroup2)",
                    controllers.display()
                ));
            }
            // 尝试建/删探测目录,验证写权限(需 root 或已委托)
            let probe = root.join(format!(".gpu-probe-{}", std::process::id()));
            std::fs::create_dir(&probe)
                .map_err(|e| format!("无法在 {} 下创建 cgroup({e});daemon 需 root 或 cgroup 委托", root.display()))?;
            let _ = std::fs::remove_dir(&probe);
            Ok(Self {
                root: root.to_path_buf(),
                progs: Mutex::new(HashMap::new()),
            })
        }

        fn dir(&self, lease_id: &str) -> PathBuf {
            // lease_id 为 UUID,无路径穿越风险;仍做防御
            let leaf: String = lease_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            self.root.join("gpu-manager").join(format!("lease-{leaf}"))
        }

        fn open_cgroup_file(&self, lease_id: &str) -> Result<std::fs::File, CgroupError> {
            use std::os::unix::fs::OpenOptionsExt;
            let dir = self.dir(lease_id);
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(&dir)
                .map_err(CgroupError::from)
        }
    }

    impl DeviceCgroup for V2Cgroup {
        fn create(&self, lease_id: &str, devices: &[DevSpec]) -> Result<(), CgroupError> {
            let parent = self.root.join("gpu-manager");
            std::fs::create_dir_all(&parent).map_err(CgroupError::from)?;
            let dir = self.dir(lease_id);
            std::fs::create_dir(&dir).map_err(CgroupError::from)?;
            let fd = self.open_cgroup_file(lease_id)?;
            let prog_fd = super::bpf::attach_device_filter(fd.as_raw_fd(), devices)
                .map_err(|e| CgroupError(format!("attach 设备过滤失败: {e}")))?;
            self.progs.lock().expect("progs 锁").insert(lease_id.to_string(), prog_fd);
            Ok(())
        }

        fn add_pid(&self, lease_id: &str, pid: u32) -> Result<(), CgroupError> {
            let procs = self.dir(lease_id).join("cgroup.procs");
            std::fs::write(procs, pid.to_string()).map_err(|e| {
                if lease_id.is_empty() {
                    CgroupError("空 lease_id".into())
                } else {
                    CgroupError(e.to_string())
                }
            })
        }

        fn kill_all(&self, lease_id: &str) -> Result<(), CgroupError> {
            let kill = self.dir(lease_id).join("cgroup.kill");
            match std::fs::write(kill, "1") {
                Ok(()) => Ok(()),
                // cgroup 可能已被清理/从未创建:当作已达目标
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(CgroupError(e.to_string())),
            }
        }

        fn destroy(&self, lease_id: &str) -> Result<(), CgroupError> {
            // 先 detach 并关闭过滤程序(尽力而为),再删目录
            let prog_fd = self.progs.lock().expect("progs 锁").remove(lease_id);
            if let (Ok(fd), Some(prog_fd)) = (self.open_cgroup_file(lease_id), prog_fd) {
                let _ = super::bpf::detach_device_filter(fd.as_raw_fd(), prog_fd);
                unsafe { libc::close(prog_fd) };
            }
            match std::fs::remove_dir(self.dir(lease_id)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                // EBUSY:进程尚未被 wait,cgroup 非空;由重试/后续清理接管
                Err(e) => Err(CgroupError(format!(
                    "删除 cgroup 失败(可能仍有僵尸成员): {e}"
                ))),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod v2 {
    use super::*;
    use std::path::Path;

    /// 非 Linux 平台占位:probe 恒失败(明确报错)
    pub struct V2Cgroup;

    impl V2Cgroup {
        pub fn probe(_root: &Path) -> Result<Self, String> {
            Err("cgroup v2 仅支持 Linux".into())
        }
    }

    impl DeviceCgroup for V2Cgroup {
        fn create(&self, _l: &str, _d: &[DevSpec]) -> Result<(), CgroupError> {
            Err(CgroupError("非 Linux 平台不支持".into()))
        }
        fn add_pid(&self, _l: &str, _p: u32) -> Result<(), CgroupError> {
            Err(CgroupError("非 Linux 平台不支持".into()))
        }
        fn kill_all(&self, _l: &str) -> Result<(), CgroupError> {
            Err(CgroupError("非 Linux 平台不支持".into()))
        }
        fn destroy(&self, _l: &str) -> Result<(), CgroupError> {
            Err(CgroupError("非 Linux 平台不支持".into()))
        }
    }
}

pub use v2::V2Cgroup;

// ---------------------------------------------------------------------------
// 设备过滤 eBPF 程序(仅 Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod bpf {
    use super::DevSpec;

    /// bpf(2) 系统调用命令
    const BPF_PROG_LOAD: i32 = 5;
    const BPF_PROG_ATTACH: i32 = 7;
    const BPF_PROG_DETACH: i32 = 8;

    /// BPF_PROG_TYPE_CGROUP_DEVICE
    const PROG_TYPE_CGROUP_DEVICE: u32 = 15;
    /// bpf_attach_type::BPF_CGROUP_DEVICE
    const ATTACH_TYPE_CGROUP_DEVICE: u32 = 4;

    /// eBPF 指令(code | dst:src 打包)
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Insn {
        code: u8,
        dst_src: u8, // 高 4 位 dst,低 4 位 src
        off: i16,
        imm: i32,
    }

    impl Insn {
        const fn new(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Self {
            Self {
                code,
                dst_src: (dst << 4) | src,
                off,
                imm,
            }
        }
        /// rX = *(u32 *)(rY + off)
        const fn ldx_w(dst: u8, src: u8, off: i16) -> Self {
            Self::new(0x61, dst, src, off, 0) // BPF_LDX|BPF_MEM|BPF_W
        }
        /// if rX != imm goto +off (64 位比较)
        const fn jne_imm(dst: u8, imm: i32, off: i16) -> Self {
            Self::new(0x55, dst, 0, off, imm) // BPF_JMP|BPF_JNE
        }
        /// rX &= imm (32 位)
        const fn and_imm(dst: u8, imm: i32) -> Self {
            Self::new(0x54, dst, 0, 0, imm) // BPF_ALU|BPF_AND
        }
        /// r0 = imm
        const fn mov64_imm(dst: u8, imm: i32) -> Self {
            Self::new(0xb7, dst, 0, 0, imm) // BPF_ALU64|BPF_MOV
        }
        /// exit(imm 为返回值)
        const fn exit(imm: i32) -> Self {
            Self::new(0x95, 0, 0, 0, imm) // BPF_JMP|BPF_EXIT
        }
    }

    /// 生成 cgroup_device 过滤程序:白名单内 allow(1),否则 deny(0)。
    ///
    /// ctx(内核):`struct bpf_cgroup_dev_ctx { access_type:u32; major:u32; minor:u32; }`
    fn build_program(allow: &[DevSpec]) -> Vec<Insn> {
        const R1: u8 = 1; // ctx
        let mut prog = vec![
            Insn::ldx_w(6, R1, 0), // r6 = access_type
            Insn::ldx_w(7, R1, 4), // r7 = major
            Insn::ldx_w(8, R1, 8), // r8 = minor
        ];
        for dev in allow {
            let start = prog.len();
            prog.push(Insn::jne_imm(7, dev.major as i32, 0)); // 位置 start+0
            prog.push(Insn::jne_imm(8, dev.minor as i32, 0)); // start+1
            prog.push(Insn::and_imm(6, dev.access as i32)); // start+2: r6 &= access
            prog.push(Insn::jne_imm(6, 0, 0)); // start+3: access 位为空 → 跳过
            prog.push(Insn::mov64_imm(0, 1)); // allow
            prog.push(Insn::exit(1));
            let next = prog.len() as i16;
            // JNE 的 off 相对下一条指令
            prog[start].off = next - (start as i16 + 1);
            prog[start + 1].off = next - (start as i16 + 2);
            prog[start + 3].off = next - (start as i16 + 4);
        }
        prog.push(Insn::mov64_imm(0, 0)); // 默认 deny
        prog.push(Insn::exit(0));
        prog
    }

    /// bpf(2) 封装。attr 以 `[u64; N]` 手工按偏移填充(内核按字节读取)。
    unsafe fn bpf_sys(cmd: i32, attr: &[u64]) -> std::io::Result<i64> {
        // SAFETY: 原生系统调用;attr 指向本函数内存,内核仅在调用期间读取
        let rc = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                cmd,
                attr.as_ptr() as *const libc::c_void,
                (attr.len() * 8) as u32,
            )
        };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    /// 加载过滤程序并 attach 到 cgroup(cgroup_fd)。成功返回 prog fd。
    ///
    /// # Safety
    ///
    /// 仅由 L4 内核接口调用;prog/license 内存需在系统调用期间存活。
    unsafe fn load_and_attach(cgroup_fd: i32, allow: &[DevSpec]) -> std::io::Result<i32> {
        let prog = build_program(allow);
        let license = b"GPL\0";
        let mut attr = [0u64; 16];
        // BPF_PROG_LOAD
        attr[0] = PROG_TYPE_CGROUP_DEVICE as u64 | ((prog.len() as u64) << 32);
        attr[1] = prog.as_ptr() as u64;
        attr[2] = license.as_ptr() as u64;
        attr[3] = (64u64) << 32; // log_size;log_buf 为 0 时内核忽略 log_level
        // log_buf(0x20) 留 0:失败时无日志,仅返回 errno(可接受)
        // SAFETY: bpf_sys 为原生系统调用封装
        let prog_fd = unsafe { bpf_sys(BPF_PROG_LOAD, &attr)? } as i32;

        // BPF_PROG_ATTACH
        let mut att = [0u64; 8];
        att[0] = cgroup_fd as u64 | ((prog_fd as u64) << 32);
        att[1] = ATTACH_TYPE_CGROUP_DEVICE as u64;
        // SAFETY: 同上
        if let Err(e) = unsafe { bpf_sys(BPF_PROG_ATTACH, &att) } {
            // attach 失败时关闭 prog fd,避免泄漏
            unsafe { libc::close(prog_fd) };
            return Err(e);
        }
        Ok(prog_fd)
    }

    /// 创建 cgroup 设备过滤并 attach;返回 prog fd(由调用方负责 detach/close)
    pub fn attach_device_filter(cgroup_fd: i32, devices: &[DevSpec]) -> std::io::Result<i32> {
        // SAFETY: 封装原生内核接口;调用方保证 cgroup_fd 有效
        unsafe { load_and_attach(cgroup_fd, devices) }
    }

    /// detach 指定过滤程序
    pub fn detach_device_filter(cgroup_fd: i32, prog_fd: i32) -> std::io::Result<()> {
        let mut att = [0u64; 8];
        att[0] = cgroup_fd as u64 | ((prog_fd as u64) << 32);
        att[1] = ATTACH_TYPE_CGROUP_DEVICE as u64;
        // SAFETY: 原生系统调用封装
        unsafe {
            bpf_sys(BPF_PROG_DETACH, &att)?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn program_layout_smoke() {
            let p = build_program(&[
                DevSpec::new(195, 0),
                DevSpec::new(1, 3), // /dev/null
            ]);
            // 3 条 ctx 读取 + 每设备 6 条 + 2 条收尾
            assert_eq!(p.len(), 3 + 6 * 2 + 2);
            // 第一条 JNE 应跳过本设备的后续 4 条(到下一设备开头)
            assert_eq!(p[3].off, (3 + 6 - 4) as i16);
            // 默认分支 deny
            let last = *p.last().unwrap();
            assert_eq!(last.code, 0x95);
        }

        #[test]
        fn empty_whitelist_denies_all() {
            let p = build_program(&[]);
            assert_eq!(p.len(), 5); // 3 ctx + r0=0 + exit
        }
    }
}

// ---------------------------------------------------------------------------
// 白名单构建
// ---------------------------------------------------------------------------

/// 为一批已分配设备构建 cgroup 设备访问白名单。
///
/// 包含:通用基础设备(/dev/null、zero、random、urandom、tty、ptmx)+
/// NVIDIA 控制/UV M 设备(Linux 下自 /proc/devices、/proc/misc 发现)+
/// 各卡的 (nvidia_major, minor)。无 NVIDIA 驱动时仅基础设备
/// (仅配合 Noop/mock 环境使用,真机需驱动)。
pub fn whitelist_for(devices: &[DeviceSnapshot]) -> Vec<DevSpec> {
    // linux 分支会 push,其它平台仅构造
    #[allow(unused_mut)]
    let mut list = vec![
        DevSpec::new(1, 3),  // /dev/null
        DevSpec::new(1, 5),  // /dev/zero
        DevSpec::new(1, 8),  // /dev/random
        DevSpec::new(1, 9),  // /dev/urandom
        DevSpec::new(5, 0),  // /dev/tty
        DevSpec::new(5, 2),  // /dev/ptmx (pty)
    ];

    #[cfg(target_os = "linux")]
    {
        if let Some(nvidia_major) = nvidia_major() {
            // nvidiactl(固定 minor 255)
            list.push(DevSpec::new(nvidia_major, 255));
            // nvidia-uvm(杂项设备,主设备号 10)
            if let Some(minor) = proc_misc_minor("nvidia-uvm") {
                list.push(DevSpec::new(10, minor));
            }
            // 各已分配卡(主设备号为驱动 major)
            for d in devices {
                if let Some(minor) = d.unix_minor {
                    list.push(DevSpec::new(nvidia_major, minor));
                } else {
                    tracing::warn!(
                        "设备 {} 缺少 unix minor,无法加入 cgroup 白名单(该卡将不可访问)",
                        d.uuid
                    );
                }
            }
        } else {
            tracing::warn!(
                "未发现 NVIDIA 驱动 major(/proc/devices 无 nvidia 项),cgroup 白名单仅含基础设备"
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = devices; // 非 Linux 无真机设备;白名单仅基础设备(Noop-only)
    }

    list
}

/// 从 /proc/devices 解析 NVIDIA 驱动主设备号
#[cfg(target_os = "linux")]
fn nvidia_major() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/devices").ok()?;
    s.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let maj: u32 = it.next()?.parse().ok()?;
        let name = it.next()?;
        (name == "nvidia").then_some(maj)
    })
}

/// 从 /proc/misc 解析杂项设备次设备号(如 nvidia-uvm)
#[cfg(target_os = "linux")]
fn proc_misc_minor(name: &str) -> Option<u32> {
    let s = std::fs::read_to_string("/proc/misc").ok()?;
    s.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let min: u32 = it.next()?.parse().ok()?;
        let n = it.next()?;
        (n == name).then_some(min)
    })
}

// ---------------------------------------------------------------------------
// 模式选择
// ---------------------------------------------------------------------------

/// cgroup 启用模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CgroupMode {
    /// 探测成功用 V2,否则 Noop(默认)
    #[default]
    Auto,
    /// 强制真机隔离;探测失败直接报错退出(fail-fast)
    V2,
    /// 强制不隔离(测试/开发)
    Noop,
}

/// 按模式选择 cgroup 实现。`v2` 模式探测失败返回 Err(daemon fail-fast);
/// `auto` 失败降级 Noop 并返回告警说明。
pub fn select_cgroup(
    mode: CgroupMode,
    root: &Path,
) -> Result<(Box<dyn DeviceCgroup>, Option<String>), String> {
    match mode {
        CgroupMode::Noop => Ok((Box::new(NoopCgroup), None)),
        CgroupMode::V2 => {
            let cg = V2Cgroup::probe(root)?;
            Ok((Box::new(cg), None))
        }
        CgroupMode::Auto => match V2Cgroup::probe(root) {
            Ok(cg) => Ok((Box::new(cg), None)),
            Err(reason) => {
                let warn = format!("cgroup v2 不可用,已降级为不隔离: {reason}");
                Ok((Box::new(NoopCgroup), Some(warn)))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用假 cgroup:记录调用,验证生命周期顺序(无卡环境验收手段)
    #[derive(Debug, Default)]
    pub struct FakeCgroup {
        pub calls: std::sync::Mutex<Vec<String>>,
    }

    impl DeviceCgroup for FakeCgroup {
        fn create(&self, lease: &str, devs: &[DevSpec]) -> Result<(), CgroupError> {
            self.calls.lock().unwrap().push(format!(
                "create {lease} {}dev",
                devs.len()
            ));
            Ok(())
        }
        fn add_pid(&self, lease: &str, pid: u32) -> Result<(), CgroupError> {
            self.calls.lock().unwrap().push(format!("add_pid {lease} {pid}"));
            Ok(())
        }
        fn kill_all(&self, lease: &str) -> Result<(), CgroupError> {
            self.calls.lock().unwrap().push(format!("kill_all {lease}"));
            Ok(())
        }
        fn destroy(&self, lease: &str) -> Result<(), CgroupError> {
            self.calls.lock().unwrap().push(format!("destroy {lease}"));
            Ok(())
        }
    }

    #[test]
    fn noop_never_fails() {
        let c = NoopCgroup;
        assert!(c.create("l1", &[]).is_ok());
        assert!(c.add_pid("l1", 1).is_ok());
        assert!(c.kill_all("l1").is_ok());
        assert!(c.destroy("l1").is_ok());
    }

    #[test]
    fn whitelist_includes_basic_devices() {
        let wh = whitelist_for(&[]);
        assert!(wh.contains(&DevSpec::new(1, 3)));
        assert!(wh.contains(&DevSpec::new(5, 2)));
    }

    #[test]
    fn fake_cgroup_tracks_lifecycle() {
        let c = FakeCgroup::default();
        c.create("lease-1", &[DevSpec::new(195, 0)]).unwrap();
        c.add_pid("lease-1", 42).unwrap();
        c.kill_all("lease-1").unwrap();
        c.destroy("lease-1").unwrap();
        let calls = c.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], "create lease-1 1dev");
        assert_eq!(calls[2], "kill_all lease-1");
    }
}