//! 网关启动失败的分类与描述（尤其是端口冲突）。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 网关默认监听 127.0.0.1:3065，启动时 bind 失败是用户最容易撞上的故障。
//! 但「失败」有两种性质完全不同的原因，处置方式正好相反：
//!   · 端口被**另一个进程**占着（`WSAEADDRINUSE` / 10048）——结束那个进程即可；
//!   · 端口落在**系统保留段**里（`WSAEACCES` / 10013）——本机没有任何进程在
//!     监听它（`netstat` 查不到东西），杀进程无从谈起，只能换端口。
//! 界面若只给一句「端口被占用」，第二种情况的用户会去找一个并不存在的进程；
//! 反过来把第一种说成「系统保留」，又会让人白白放弃一条有效的出路。
//!
//! ── 判据为什么是错误码，而不是 netsh ────────────────────────
//! `netsh int ipv4 show excludedportrange protocol=tcp` 能列出保留段，但它
//! **不能当判据**：实测同一台机器上，列在保留段里的端口有的照样能 bind 成功
//! （7901-8000、50000-50059 这两段里的端口实测可绑），只有其中一部分会被拒。
//! 也就是说「出现在 netsh 列表里」既不充分也不必要，真正的判据只有 bind 失败
//! 的错误码本身 —— 而 `std::io::ErrorKind` 已经把两者映射成了不同的值
//! （10048 → `AddrInUse`，10013 → `PermissionDenied`）。
//!
//! 不 spawn 外部命令还有另一层考虑：见 backend.rs 里关于杀软启发式的说明 ——
//! 「监听本地端口」叠加「创建进程执行 netstat/netsh」正是流量拦截类木马的特征。
//!
//! ── 本模块只描述，不动作 ────────────────────────────────────
//! 查监听进程、结束进程树这类动作都在 backend.rs（那边才有 Win32 调用），
//! 本模块只负责把事实翻译成「性质 + 处置建议」，好让壳侧与界面共用同一套说法
//! （界面不能自己拼文案：同一件事在侧栏、弹窗、日志里说法不一致时，
//! 用户无从判断哪个是真的）。

use serde::Serialize;

/// 端口冲突的性质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictKind {
    /// 有另一个进程在监听：结束那个进程就能把端口拿回来。
    Occupied,
    /// 被系统保留 / 被安全策略拒绝：本机没有任何进程在监听，只能换端口。
    Reserved,
    /// 其它 bind 失败原因（地址不可用、权限不足等）：没有确定的处置手段。
    Other,
}

/// 占用端口的进程。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Occupant {
    pub pid: u32,
    /// 进程名（取自可执行文件路径的文件名）
    pub name: String,
    /// 可执行文件绝对路径；查不到时为空串
    pub path: String,
    /// 是不是本进程自己（界面据此说「网关已在运行」，而不是「被别人占了」）
    pub is_self: bool,
    /// 可执行文件是否位于系统目录（`%SystemRoot%`）之内。
    ///
    /// 这类进程（svchost 等）不该由我们结束：既可能承载着别的服务，
    /// 杀了也未必释放端口（端口可能是被内核组件代理的）。界面据此只给「更换端口」。
    pub system: bool,
}

/// 一次端口冲突的完整描述。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortConflict {
    pub kind: ConflictKind,
    pub port: u16,
    /// 给人看的完整说明（含处置建议）
    pub message: String,
    pub occupant: Option<Occupant>,
    /// 原始 OS 错误文本（不发给界面，只用于补全 occupant 后重算文案）
    #[serde(skip)]
    detail: String,
}

impl PortConflict {
    /// 从 bind 失败的错误构造冲突描述。
    ///
    /// `occupant` 由调用方查（backend 里才有查监听进程的 Win32 代码），
    /// 传 None 表示「没查」或「查不到」——`Reserved` 本来就查不到进程。
    pub fn from_bind_error(port: u16, error: &std::io::Error, occupant: Option<Occupant>) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::AddrInUse => ConflictKind::Occupied,
            std::io::ErrorKind::PermissionDenied => ConflictKind::Reserved,
            _ => ConflictKind::Other,
        };
        Self::new(kind, port, occupant, &error.to_string())
    }

    /// 组装冲突描述（文案集中在这里，避免界面与后端各说各话）。
    pub fn new(kind: ConflictKind, port: u16, occupant: Option<Occupant>, detail: &str) -> Self {
        let message = describe(kind, port, occupant.as_ref(), detail);
        Self { kind, port, message, occupant, detail: detail.to_string() }
    }

    /// 补上监听进程信息并重算文案。
    ///
    /// bind 失败发生在 `server::start` 里（那里只知道 OS 错误码），而「谁占着端口」
    /// 要查 Win32 监听表（在 backend.rs）。分两步是因为错误信息的构造应该贴近
    /// 出错点，而进程查询属于平台细节 —— 这里把两者接起来。
    pub fn with_occupant(mut self, occupant: Option<Occupant>) -> Self {
        self.occupant = occupant;
        self.message = describe(self.kind, self.port, self.occupant.as_ref(), &self.detail);
        self
    }

    /// 这个冲突能不能靠「结束进程」解决。
    ///
    /// 判据不止看 kind：系统目录里的进程、本进程自己，都不该由界面提供结束操作。
    /// 界面据此决定按钮的显隐 —— 给一个点了必然失败（或不该成功）的按钮，
    /// 比不给更糟。
    pub fn can_end_occupant(&self) -> bool {
        match &self.occupant {
            Some(who) => self.kind == ConflictKind::Occupied && !who.is_self && !who.system,
            None => false,
        }
    }

    /// 侧栏那一行用的短标签。
    ///
    /// 与 `message` 分开是因为长度约束完全不同：状态条只有一行、还要带端口号，
    /// 完整说明（可能上百字）只能进 title 与弹窗。两处措辞都由后端给，
    /// 是为了避免界面自己拼一套说法、和后端日志里的说法对不上。
    pub fn label(&self) -> &'static str {
        match self.kind {
            ConflictKind::Occupied => "端口被占用",
            ConflictKind::Reserved => "端口被系统保留",
            ConflictKind::Other => "网关启动失败",
        }
    }
}

/// 按性质生成说明文案（`new` 与 `with_occupant` 共用，保证两处口径一致）
fn describe(kind: ConflictKind, port: u16, occupant: Option<&Occupant>, detail: &str) -> String {
    match kind {
        ConflictKind::Occupied => match occupant {
            Some(who) if who.is_self => format!(
                "端口 {port} 正被本程序占用（PID {}）。网关已在运行，无需重复启动。",
                who.pid
            ),
            Some(who) => format!(
                "端口 {port} 被 {} 占用（PID {}）。结束该进程即可拿回端口。",
                who.name, who.pid
            ),
            None => {
                format!("端口 {port} 已被占用，但查不到是哪个进程在监听（可能是权限不足）。")
            }
        },
        ConflictKind::Reserved => format!(
            "端口 {port} 被系统保留，本机没有任何进程在监听它，因此结束进程无法解决。\
             这类保留段通常由 Hyper-V / WSL2 / Docker 的虚拟网卡服务（winnat、hns）在\
             开机时划分，重启电脑只是碰运气（可能换一段保留，也可能照样撞上）。\
             请改用其它端口。"
        ),
        ConflictKind::Other => format!("端口 {port} 无法监听：{detail}"),
    }
}

/// 网关启动失败。
///
/// 壳侧把它原样发给界面（`backend:error` 事件 + `backend_status` 命令），
/// 界面据此决定：只显示说明，还是再给出「结束占用进程」/「更换端口」的出口。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupFailure {
    /// 给人看的完整说明
    pub message: String,
    /// 侧栏那一行用的短标签（「端口被占用」这类）；界面不自己拼，保证与后端一致
    pub label: String,
    /// 端口冲突详情；非端口原因（配置迁移失败、健康检查超时）时为 None
    pub conflict: Option<PortConflict>,
    /// 这个失败能不能靠结束进程解决（冗余字段，省得前端在嵌套结构里翻）
    pub can_end_occupant: bool,
}

impl StartupFailure {
    /// 端口没能监听成功
    pub fn port(conflict: PortConflict) -> Self {
        Self {
            message: conflict.message.clone(),
            label: conflict.label().to_string(),
            can_end_occupant: conflict.can_end_occupant(),
            conflict: Some(conflict),
        }
    }

    /// 其它原因的启动失败（配置迁移、健康检查等），没有除「看日志」以外的处置手段
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            label: "网关启动失败".to_string(),
            conflict: None,
            can_end_occupant: false,
        }
    }
}
