//! 一次性配置目录迁移：`~/.workbuddy-proxy` → `~/.agent2api`（只拷贝，不删源）。
//!
//! ── 语义（架构文档 §3.1）────────────────────────────────────
//!   - 新目录不存在、旧目录存在 → 把旧目录**整体拷贝**为新目录，旧目录保留不动
//!     （用户可随时回退到旧版，且迁移出问题时数据仍在原处）；
//!   - 两者都存在 → 什么都不做（**以新目录为准**，不合并、不覆盖 ——
//!     合并会让「哪些数据以哪边为准」变得不可预测）。已存在的新目录一律当作
//!     用户/前一次运行的真实数据，**不猜测**它是历史失败产物、更不覆写；
//!   - 都不存在 → 什么都不做（后续初始化自己会建空目录）；
//!   - 有环境变量覆盖（`AGENT2API_PROXY_HOME` / 旧名）→ 什么都不做：用户显式
//!     指定了目录，这时去动默认路径只会制造困惑，也可能把数据拷到错误的盘上。
//!
//! ── 为什么是拷贝而不是改名 ───────────────────────────────────
//! 改名（rename）在两端目录位于不同卷时不可靠，而且会让已装旧版的用户
//! 回退旧版本时找不到数据。配置目录只有 config.json / accounts.json /
//! logs.jsonl 这类小文件，拷贝的代价可以忽略。
//!
//! ── 暂存目录：本次唯一 + 亲自创建，才允许清理 ─────────────────
//! 直接往新目录里拷，一旦中途失败（磁盘满 / 某个文件被占用）就会留下一个
//! **残缺的新目录**；下次启动看到「新目录已存在」就走「以新目录为准」分支，
//! 再也不会重试 —— 用户的账号列表会莫名其妙少一半，且无从恢复。
//! 因此先拷到同级暂存目录，成功后再 `rename` 成正式目录（两者同卷，改名原子）：
//! 新目录要么是完整副本，要么根本不存在（下次启动自动重试）。
//!
//! 暂存目录名带本次进程号与时间戳（`{正式目录名}.migrating-{pid}-{nanos}`），
//! 用 `create_dir` 亲自创建、并先写入所有权标记文件，于是：
//!   - 失败时只删**本次自己创建**的那一个路径，绝不对未知的同名固定路径
//!     调 `remove_dir_all`；
//!   - 崩溃残留的旧暂存目录，只有在「名字带本程序前缀 **且** 标记文件存在」时
//!     才清理，其余一律只提示不动手 —— 宁可能留垃圾，也不误删用户数据。
//!
//! ── 链接 / 重解析点不跟随 ───────────────────────────────────
//! 拷贝时遇到符号链接、Windows 重解析点（junction / mount point）一律跳过并
//! 提示：跟随它们会遍历到配置目录之外（甚至别的卷），既可能拷进无关数据，
//! 也可能踩到环路。旧目录本身若就是链接/重解析点，直接报错拒绝迁移。
//!
//! ── 失败会阻断本次启动（且可重试）──────────────────────────
//! 返回 `Result`：调用方（壳侧 `lib.rs` 的 setup 第一步）失败时**不写任何配置
//! 文件**、提示用户后退出。这样旧目录原样保留、新目录不会被建出来，下一次
//! 启动仍会重试。若失败后继续启动，后续任何一次写盘（桌面设置 / config.json /
//! 日志库）都会把新目录建出来，迁移从此再也不会发生，用户看到的是「数据消失」。
//!
//! 错误信息只带路径与系统错误，不含文件内容（配置里有 token，不进日志/对话框）。

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

/// 暂存目录名后缀（正式目录名 + 后缀 + 本次唯一标识，见模块头）
const STAGING_SUFFIX: &str = ".migrating";

/// 暂存目录的所有权标记文件：只有带它的暂存目录才被认为是本程序创建的。
/// 名字取得足够特别，避免与用户自己的文件重名后被误判。
const STAGING_MARKER: &str = ".agent2api.migrating.marker";

/// 迁移结果（成功时的分支；调用方据此决定要不要提示用户）
#[derive(Debug)]
pub enum MigrationOutcome {
    /// 已完成迁移（源目录保留）
    Migrated { from: PathBuf, to: PathBuf },
    /// 无需迁移（新目录已存在 / 旧目录不存在）
    NothingToDo,
    /// 用户用环境变量显式指定了配置目录，迁移不适用
    SkippedEnvOverride,
}

/// 迁移配置目录。失败返回可读的中文说明（调用方据此提示用户并退出）。
///
/// 幂等：迁移成功后新目录存在，后续每次启动都会在第一道判断上直接返回。
///
/// 成功分支不在这里打日志：用户可见的启动日志由调用方（壳侧 setup）统一输出，
/// 那里也正好能拿到 `Migrated` 的源/目标路径。
pub fn migrate_config_dir() -> Result<MigrationOutcome, String> {
    // 有环境变量覆盖（新名或旧名）时不迁移：用户显式指定了目录
    if env_dir_override_set() {
        return Ok(MigrationOutcome::SkippedEnvOverride);
    }

    let target = paths::config_dir();
    // 新目录存在（哪怕是链接）→ 以它为准：不合并、不覆盖、不猜测来源
    if std::fs::symlink_metadata(&target).is_ok() {
        return Ok(MigrationOutcome::NothingToDo);
    }

    let legacy = crate::server::config::legacy_config_dir();
    let Ok(legacy_metadata) = std::fs::symlink_metadata(&legacy) else {
        return Ok(MigrationOutcome::NothingToDo);
    };
    if is_link_like(&legacy_metadata) {
        return Err(format!(
            "旧配置目录 {} 是链接或 Windows 重解析点，出于安全考虑不跟随迁移。\
             请把它改成真实目录后重启，或手动把数据拷到 {}。",
            legacy.display(),
            target.display()
        ));
    }
    if !legacy_metadata.is_dir() {
        return Err(format!(
            "旧配置路径 {} 不是目录（可能被同名文件占住），无法迁移。\
             请先处理该路径后重启；正式配置目录应为 {}。",
            legacy.display(),
            target.display()
        ));
    }

    let Some(parent) = target.parent() else {
        return Err(format!(
            "配置目录 {} 没有父目录，无法迁移（旧数据仍在 {}）。",
            target.display(),
            legacy.display()
        ));
    };
    let Some(target_name) = target.file_name().and_then(|name| name.to_str()) else {
        return Err(format!(
            "配置目录名 {} 不是合法 UTF-8，无法迁移（旧数据仍在 {}）。",
            target.display(),
            legacy.display()
        ));
    };

    // 上次迁移中途崩溃可能留下暂存目录：只清理能确认属于本程序的那些
    sweep_stale_staging(parent, target_name);

    let staging = parent.join(staging_name(target_name));
    // create_dir（不是 create_dir_all）：目录必须由本次调用创建成功，
    // 否则说明撞名/被别人占了，宁可报错也不能往里面拷
    if let Err(error) = std::fs::create_dir(&staging) {
        return Err(format!(
            "创建迁移暂存目录 {} 失败：{error}。旧数据仍完整保留在 {}；\
             修复后重启本程序会自动重试。",
            staging.display(),
            legacy.display()
        ));
    }

    match publish(&legacy, &staging, &target) {
        Ok(Publish::Published) => Ok(MigrationOutcome::Migrated { from: legacy, to: target }),
        // 拷贝期间正式目录被别的进程建出来：以已存在的目录为准，本次不再迁移，
        // 也不算失败（旧目录同样原样保留）。清掉本次暂存目录即可。
        Ok(Publish::TargetAppeared) => {
            remove_own_staging(&staging);
            Ok(MigrationOutcome::NothingToDo)
        }
        Err(error) => {
            remove_own_staging(&staging);
            Err(error)
        }
    }
}

/// 目录迁移是否**仍未完成**（防御性校验，供 `ServerState::bootstrap` 使用）。
///
/// 判定条件与 `migrate_config_dir` 的触发条件完全一致：无环境变量覆盖、
/// 旧目录存在、新目录不存在。满足时说明「本该执行的迁移没有执行」——
/// 此时若继续启动，后端随后任何一次写盘（日志库 / config.json / 账号库）
/// 都会把新目录建出来，迁移从此再也无法重试，用户看到的是「数据消失」。
/// 返回 `Some(原因)` 时调用方应当拒绝启动。
pub fn pending_reason() -> Option<String> {
    if env_dir_override_set() {
        return None;
    }
    let target = paths::config_dir();
    if std::fs::symlink_metadata(&target).is_ok() {
        return None;
    }
    let legacy = crate::server::config::legacy_config_dir();
    if std::fs::symlink_metadata(&legacy).is_err() {
        return None;
    }
    Some(format!(
        "检测到旧配置目录 {} 尚未迁移到 {}，但本次启动没有执行目录迁移。\
         为避免以空配置继续运行（那会让迁移再也无法重试、旧数据看似丢失），\
         本次网关初始化已停止。请重启本程序重试；若反复出现请反馈该问题。",
        legacy.display(),
        target.display()
    ))
}

/// 发布结果：正常改名，或「拷贝期间正式目录被别的进程建出来」。
enum Publish {
    /// 暂存目录已原子改名为正式目录
    Published,
    /// 正式目录在拷贝期间出现：以它为准，本次不改名覆盖
    TargetAppeared,
}

/// 拷贝到暂存目录 → 去掉标记 → 原子改名成正式目录。
///
/// 任何一步失败都返回中文说明；调用方负责清掉本次创建的暂存目录。
fn publish(legacy: &Path, staging: &Path, target: &Path) -> Result<Publish, String> {
    // 标记文件先于任何拷贝写入：它是「这个目录由本次迁移创建」的证据，
    // 后续清理（含下次启动扫残留）只认它
    let marker = staging.join(STAGING_MARKER);
    if let Err(error) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        return Err(format!(
            "在暂存目录 {} 写入标记文件失败：{error}。旧数据仍完整保留在 {}；\
             修复后重启本程序会自动重试。",
            staging.display(),
            legacy.display()
        ));
    }

    if let Err(error) = copy_dir_all(legacy, staging, &marker) {
        return Err(format!(
            "拷贝 {} → {} 失败：{error}。旧数据仍完整保留在 {}；\
             修复后（例如关闭占用文件的程序、释放磁盘空间）重启本程序会自动重试，\
             本次未改动或删除旧目录。",
            legacy.display(),
            target.display(),
            legacy.display()
        ));
    }

    // 标记文件不能跟着进正式目录（否则新配置目录里会多一个无意义的文件）
    if let Err(error) = std::fs::remove_file(&marker) {
        return Err(format!(
            "清理暂存目录标记文件 {} 失败：{error}。旧数据仍完整保留在 {}；\
             修复后重启本程序会自动重试。",
            marker.display(),
            legacy.display()
        ));
    }

    // 同卷 rename 是原子的：要么新目录完整出现，要么根本没有（下次重试）。
    // 拷贝期间若正式目录被别的进程建出来（理论上被单实例插件挡住，这里仍兜一手），
    // 直接失败而不是改名覆盖 —— 已有目录一律以它为准，绝不擅自覆盖用户数据。
    if std::fs::symlink_metadata(target).is_ok() {
        crate::server::logging::console_line(
            "[Config]",
            &format!(
                "⚠️  拷贝期间配置目录 {} 已出现，本次不改名覆盖（以已存在的目录为准；旧数据仍在 {}）",
                target.display(),
                legacy.display()
            ),
        );
        return Ok(Publish::TargetAppeared);
    }
    std::fs::rename(staging, target)
        .map(|()| Publish::Published)
        .map_err(|error| {
            format!(
                "把暂存目录 {} 改名为 {} 失败：{error}。旧数据仍完整保留在 {}；\
                 修复后重启本程序会自动重试。",
                staging.display(),
                target.display(),
                legacy.display()
            )
        })
}

/// 删除本次自己创建的暂存目录；失败只提示（它不影响源数据与下次重试）。
fn remove_own_staging(staging: &Path) {
    if let Err(error) = std::fs::remove_dir_all(staging) {
        crate::server::logging::console_line(
            "[Config]",
            &format!(
                "⚠️  清理本次迁移的暂存目录 {} 失败（不影响旧数据，下次启动会重试迁移）: {error}",
                staging.display()
            ),
        );
    }
}

/// 本次运行唯一的暂存目录名：`{正式目录名}.migrating-{pid}-{nanos}`。
///
/// 带上进程号与纳秒时间戳，既保证「这个目录一定是本次创建的」，也让崩溃残留
/// 不会与下一次运行撞名（撞名会让 create_dir 直接失败，迁移永远重试不了）。
fn staging_name(target_name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{target_name}{STAGING_SUFFIX}-{}-{nanos}", std::process::id())
}

/// 清理**可确认属于本程序**的历史暂存目录（上次迁移中途崩溃/断电留下）。
///
/// 只处理同一父目录下、名字带本程序前缀、且带所有权标记文件的目录；
/// 不满足条件的同名路径只提示、绝不动手 —— 用户自己的目录不能被当成垃圾删掉。
/// 清理失败同样只提示：它不影响本次迁移（本次用的是另一个新名字）。
fn sweep_stale_staging(parent: &Path, target_name: &str) {
    let prefix = format!("{target_name}{STAGING_SUFFIX}");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        let owned = std::fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir() && !is_link_like(&metadata))
            .unwrap_or(false)
            && std::fs::symlink_metadata(path.join(STAGING_MARKER))
                .map(|metadata| metadata.is_file() && !is_link_like(&metadata))
                .unwrap_or(false);
        if !owned {
            crate::server::logging::console_line(
                "[Config]",
                &format!(
                    "⚠️  临时路径 {} 已存在且不是本程序创建的目录，未处理（不影响本次迁移）",
                    path.display()
                ),
            );
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => crate::server::logging::console_line(
                "[Config]",
                &format!("已清理上次迁移残留的临时目录: {}", path.display()),
            ),
            Err(error) => crate::server::logging::console_line(
                "[Config]",
                &format!(
                    "清理上次迁移残留的临时目录失败（不影响本次迁移）: {} - {error}",
                    path.display()
                ),
            ),
        }
    }
}

/// 递归拷贝目录（`std::fs` 没有现成的递归拷贝）。
///
/// 只拷贝普通文件与目录：符号链接 / Windows 重解析点（junction、mount point）
/// 一律**跳过并提示**，绝不跟随 —— 配置目录里正常不该有链接，真有时跟随它
/// 会遍历到配置目录之外（甚至别的卷），既可能拷进无关数据，也可能踩到环路。
/// 其它类型（设备、管道等）同样跳过。
///
/// `skip` 是暂存目录里的所有权标记文件路径：它只属于本次迁移，源目录里即使
/// 恰好同名也不该把它覆盖/写坏（那会让清理逻辑认不出自己的目录）。
fn copy_dir_all(from: &Path, to: &Path, skip: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if destination == skip {
            continue;
        }
        if is_link_like(&entry.metadata()?) {
            crate::server::logging::console_line(
                "[Config]",
                &format!("⚠️  迁移跳过链接/重解析点（不跟随）: {}", source.display()),
            );
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir(&destination)?;
            copy_dir_all(&source, &destination, skip)?;
        } else if file_type.is_file() {
            std::fs::copy(&source, &destination)?;
        } else {
            crate::server::logging::console_line(
                "[Config]",
                &format!("⚠️  迁移跳过非普通文件: {}", source.display()),
            );
        }
    }
    Ok(())
}

/// 是否是链接类目录项：符号链接，或 Windows 重解析点（junction / mount point）。
///
/// `file_type().is_symlink()` 只覆盖真正的符号链接；Windows 的 junction 在
/// `file_attributes` 上带 `FILE_ATTRIBUTE_REPARSE_POINT` 位，两者都要判，
/// 否则「跟随 junction 越界遍历」的洞还在。
fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        /// `FILE_ATTRIBUTE_REPARSE_POINT`。不引 windows-sys 常量：那个依赖
        /// 只在 Windows 目标下存在，写死位值可以让本文件跨平台编译。
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// 是否设置了配置目录的环境变量覆盖（`AGENT2API_PROXY_HOME` 或旧名）。
///
/// 只判断「有没有设」，不重复解析路径 —— 解析口径的唯一事实来源是
/// `paths::config_dir()`；覆盖生效时迁移整体不适用。
fn env_dir_override_set() -> bool {
    ["AGENT2API_PROXY_HOME", "WORKBUDDY_PROXY_HOME"]
        .iter()
        .any(|name| {
            std::env::var(name)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
}
