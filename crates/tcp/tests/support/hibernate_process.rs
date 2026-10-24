//! Hibernate 专项与基准的进程隔离和硬截止。
//!
//! 只管理本次显式创建的子进程组，不按进程名结束其它任务。Linux 上正常返回、
//! 超时、提前返回和展开均清理所属进程组并回收直接子进程；未清理不算测试完成。
//! 轮询 /proc 只用于诊断 CPU 推进及确认残留，不参与生产锁或运行时状态转换。
//! 本工具是直属子进程的唯一回收方；在未回收身份仍存在时完成发信号，回收后
//! 撤销清理责任。不得另装忽略SIGCHLD的处理或由其它线程抢先回收该子进程。

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

static NEXT_RUN: AtomicUsize = AtomicUsize::new(0);

/// 一次隔离执行的真实结果；超时不是成功，也不是单独充分的循环等待证据。
/// stdout/stderr 及 CPU 计数保留到 artifacts，供核对阶段和环境失败。
pub(super) struct ChildRun {
    pub status: ExitStatus,
    pub timed_out: bool,
    pub cpu_ticks: u64,
    pub stalled_after_probes: bool,
    pub stdout: String,
    pub stderr: String,
    pub artifacts: PathBuf,
}

/// 持有唯一子进程回收责任，不 Clone；Drop 是测试异常时的兜底，不用于生产关闭。
struct OwnedChild {
    child: Child,
    group: u32,
    status: Option<ExitStatus>,
}

/// Linux /proc/stat 的最小解析；返回进程组、累计CPU tick及尚未回收的退出状态。
/// 命令名允许包含空格或括号，不能按整行空格直接切分。读取与进程退出可竞争。
fn process_stat(pid: u32) -> Option<(u32, u64, bool)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, tail) = text.rsplit_once(") ")?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let group = fields.get(2)?.parse().ok()?;
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    Some((group, user.saturating_add(system), fields.first() == Some(&"Z")))
}

/// 返回准确 PGID 的成员快照；O(进程数)，只在子进程清理时使用。
fn group_members(group: u32) -> Vec<u32> {
    fs::read_dir("/proc").expect("读取测试进程目录失败")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| process_stat(*pid).is_some_and(|(current, _, _)| current == group))
        .collect()
}

impl OwnedChild {
    /// 停止整个准确进程组；组号来自尚未回收的直属子进程，不接受外部 PID。
    fn stop_group(&mut self) {
        if self.status.is_some() { return; }
        #[cfg(unix)]
        if !group_members(self.group).is_empty() {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.group)])
                .status();
        }
        let _ = self.child.kill();
    }

    /// 等待直接子进程回收并检查整个组消失；截止后仍有成员就报告清理失败。
    fn finish(&mut self) -> ExitStatus {
        if let Some(status) = self.status { return status; }
        self.stop_group();
        let status = self.child.wait().expect("回收测试子进程失败");
        // wait之后不再向旧PID/PGID发信号；即使下面检查失败并展开也不重复清理。
        self.status = Some(status);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !group_members(self.group).is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(group_members(self.group).is_empty(), "测试进程组仍有残留: {}", self.group);
        status
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.status.is_none() {
            self.stop_group();
            let _ = self.child.wait();
        }
    }
}

/// 在独立进程组内执行给定测试/基准命令，截止时强制结束所有组成员。
///
/// command 仅允许本测试构造的同一二进制；deadline 必须大于零。同步阻塞调用，
/// 不得放进业务 Future。每次生成独立原件目录，不覆盖既有结果；非纯、非幂等。
/// 这里只支持本次 Linux 验证，不能据此宣称 Windows 进程树已同等清理。
pub(super) fn run_child(command: &mut Command, deadline: Duration, tag: &str) -> ChildRun {
    assert!(cfg!(target_os = "linux"), "此进程监控目标仅在 Linux 执行");
    assert!(!deadline.is_zero());
    let root = std::env::var_os("PI_TCP_HIB_ARTIFACT_DIR")
        .map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&root).expect("创建测试归档根目录失败");
    let artifacts = root.join(format!("hibernate-{}-{}-{tag}", std::process::id(),
        NEXT_RUN.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir(&artifacts).expect("创建唯一测试归档目录失败");
    command.stdout(Stdio::from(File::create(artifacts.join("stdout.log")).unwrap()));
    command.stderr(Stdio::from(File::create(artifacts.join("stderr.log")).unwrap()));
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().expect("启动测试子进程失败");
    let group = child.id();
    let mut owned = OwnedChild { child, group, status: None };
    fs::write(artifacts.join("process.txt"), format!("pid={group}\npgid={group}\n"))
        .expect("保存子进程身份失败");
    let start = Instant::now();
    let first_ticks = process_stat(group).map_or(0, |(_, ticks, _)| ticks);
    let mut last_ticks = first_ticks;
    let mut probes_at = None;
    let mut stalled_after_probes = false;
    let (status, timed_out) = loop {
        if let Some((_, ticks, exited)) = process_stat(group) {
            last_ticks = ticks;
            // 不用try_wait探测完成，它会提前回收并解除PID占用；Z状态仍保留
            // 本工具独占的直属子进程身份，故可先结束准确组再统一wait。
            if exited { break (owned.finish(), false); }
        }
        if tag.ends_with("-overlap") {
            let progress = fs::read_to_string(artifacts.join("stdout.log")).unwrap_or_default();
            if progress.contains("阶段=poll持有W") && progress.contains("阶段=wakeup尝试W")
                && !progress.contains("阶段=两次真实回包已完成") {
                let began = probes_at.get_or_insert_with(Instant::now);
                if began.elapsed() >= Duration::from_secs(1)
                    && last_ticks.saturating_sub(first_ticks) >= 10 {
                    stalled_after_probes = true;
                    break (owned.finish(), true);
                }
            }
        }
        if start.elapsed() >= deadline {
            break (owned.finish(), true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    // finish已缓存回收结果；重复调用不再发信号，也不能遗留所属工作进程。
    let _ = owned.finish();
    let cpu_ticks = last_ticks.saturating_sub(first_ticks);
    fs::write(artifacts.join("monitor.txt"), format!(
        "timed_out={timed_out}\nstalled_after_probes={stalled_after_probes}\ncpu_ticks={cpu_ticks}\nstatus={status}\nremaining=0\n"))
        .expect("保存监控结果失败");
    ChildRun {
        status, timed_out, cpu_ticks, stalled_after_probes,
        stdout: fs::read_to_string(artifacts.join("stdout.log")).unwrap(),
        stderr: fs::read_to_string(artifacts.join("stderr.log")).unwrap(),
        artifacts,
    }
}
