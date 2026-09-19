//! 旧「当前用户」级安装（1.x）的清理：目录、快捷方式、卸载注册项，
//! 以及旧开机自启登记的识别。
//!
//! ── 为什么清理必须在应用启动时做，而不是写进 NSIS 安装脚本 ──────────────
//! 安装模式从 currentUser 改为 perMachine 后，安装器以管理员身份运行：此时
//! `%LOCALAPPDATA%` 与 HKCU 指向的是**管理员账户**，既读不到也删不掉发起安装
//! 的那个登录用户的旧安装。而新版应用启动时是普通用户上下文，`%LOCALAPPDATA%`
//! 与 HKCU 都属于当前用户，删用户目录下的文件也不需要管理员权限 —— 这是唯一
//! 能覆盖到旧安装的位置。
//!
//! ── 只删「确认是本产品」的目标 ────────────────────────────────────
//! 判定链每一环都必须成立，任何一环不成立就跳过（宁可留下垃圾，也绝不误删
//! 用户自己安装的其它软件）：
//!   目录     必须存在本产品主程序（`workbuddy-proxy-desktop.exe` / `agent2api.exe`）；
//!   卸载键   键名与 DisplayName 都要等于本产品的历史/现用名，且键里记录的安装
//!            位置要落在对应旧目录内、该目录已经不存在；
//!   快捷方式 文件名等于产品名，且 .lnk 内容里出现旧目录的绝对路径；
//!   自启值   值名等于产品名，内容里出现旧目录路径，且它指向的可执行文件已不在。
//! 另外，**当前进程所在目录永远不会被删**（用户把新版手工拷进旧目录时不能删自己）。
//!
//! ── 失败只记日志，下次启动重试 ────────────────────────────────────
//! 删除失败（文件被占用、权限不足）不阻断启动。函数是幂等的：目标不存在时
//! 所有判定直接落空，正常启动零开销。
//!
//! ── 自启登记是同一问题的另一半（见 `autostart_needs_refresh`）──────────
//! 旧登记指向 `%LOCALAPPDATA%` 下的旧目录、值名也用旧产品名；新版装在 Program
//! Files 且用 productName 登记，两者互不覆盖，旧的那条会静默失效。
//!
//! ── 日志为什么用 eprintln! ──────────────────────────────────────
//! 调用点在 setup 早期且跑在后台任务里，日志库此时可能还没初始化；
//! `server::logging::log` 的控制台通道本身也是 eprintln!，这里直接用更直白。

#[cfg(windows)]
pub use imp::{autostart_needs_refresh, cleanup_legacy_user_install};

/// 非 Windows 平台：本项目的打包目标只有 Windows，保留分支是为了其它平台
/// 也能编译（与 `update.rs` 的 `launch_installer` 同一处理）
#[cfg(not(windows))]
pub fn cleanup_legacy_user_install() {}

/// 非 Windows 平台的等价桩实现（自启登记由各平台自己处理，无需刷新）
#[cfg(not(windows))]
pub fn autostart_needs_refresh(_current_name: &str) -> bool {
    false
}

#[cfg(windows)]
mod imp {
    //! Windows 实现。

    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::registry::{self, HKEY_CURRENT_USER};

    /// 本产品用过的产品名。NSIS 在 currentUser 模式下把这些名字同时用作
    /// 安装目录名（`%LOCALAPPDATA%\<产品名>`）、卸载键名与快捷方式名。
    const PRODUCT_NAMES: [&str; 2] = ["Agent2API", "WorkBuddy 本地代理"];

    /// 旧安装目录里可能出现的主程序文件名。
    ///
    /// `workbuddy-proxy-desktop.exe` 是实际产物名：Cargo.toml 的 package name 是
    /// `workbuddy-proxy-desktop`，而 tauri.conf.json 没有设 mainBinaryName，所以
    /// NSIS 的 MAINBINARYNAME 与磁盘上的 exe 都是它（已按构建出的 installer.nsi
    /// 与真实安装目录核对）。另一个候选是防御性的：产品已改名为 Agent2API，
    /// 将来若把二进制一起改名，旧目录里就会是这个文件名，判定链不该因此失效。
    const BINARY_NAMES: [&str; 2] = ["workbuddy-proxy-desktop.exe", "agent2api.exe"];

    /// 当前用户级安装的卸载信息（NSIS 的 currentUser 模式写这里）
    const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

    /// 开机自启登记（tauri-plugin-autostart 底层的 auto-launch 用同一处）
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    /// 任务管理器「启动」页的启用状态：与 Run 同名的一个二进制值
    const STARTUP_APPROVED_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";

    /// 清理旧用户级安装。同步、幂等、失败只记日志。
    pub fn cleanup_legacy_user_install() {
        let dirs = legacy_dirs();
        if dirs.is_empty() {
            return;
        }
        // 目录先删，后面的卸载键 / 快捷方式 / 自启值都按「目录是否已不存在」判定，
        // 避免把仍然可用的旧安装入口（卸载器、快捷方式）先删掉
        let cleaned: Vec<(&'static str, PathBuf, bool)> = dirs
            .into_iter()
            .map(|(name, dir)| {
                let removed = remove_legacy_dir(&dir);
                (name, dir, removed)
            })
            .collect();

        cleanup_shortcuts(&cleaned);
        cleanup_uninstall_keys(&cleaned);
        cleanup_autostart_entries(&cleaned);
    }

    /// 是否需要按当前 exe 路径重建自启登记（值名为 `current_name`）。
    ///
    /// 场景：1.x 把登记写在 `%LOCALAPPDATA%\<旧产品名>` 下、值名也用旧产品名；
    /// 改成 perMachine 后新版装在 Program Files，登记用的值名换成 productName。
    /// 旧值指向已被删除的目录（由本模块清掉），新值则**根本不存在** ——
    /// 插件自己的 `is_enabled()` 只会如实回答「没登记」，用户不开设置页就永远
    /// 不会重建，自启静默失效。启动期据此补一次登记。
    ///
    /// 判定条件（全部满足才返回 true，保证幂等且不覆盖用户的选择）：
    ///   1. 当前值名的 Run 值不存在，或它的可执行文件不是当前进程的 exe；
    ///   2. 任务管理器「启动」页里没有针对该值名的停用记录 —— 用户显式关掉的
    ///      自启不该被我们悄悄打开（停用记录存在时 Run 值仍在，所以第 1 条
    ///      单独判断会误判成「需要重建」）。
    ///
    /// 只查这两处、不读设置文件：调用方已确认用户想要自启。
    pub fn autostart_needs_refresh(current_name: &str) -> bool {
        if startup_disabled(current_name) {
            return false;
        }
        let Some(value) = registry::read_string(HKEY_CURRENT_USER, RUN_KEY, current_name) else {
            return true;
        };
        match (exe_path_from(&value), std::env::current_exe()) {
            // 登记里记的就是当前这个可执行文件：已经是最新的，无需重建
            (Some(registered), Ok(current)) => {
                !same_path(&registered, &current)
            }
            // 值解析不出可执行文件（被改坏）：重建一次比留着一条无效登记好
            (None, _) => true,
            // 取不到当前 exe 路径（理论上不会发生）：不动它
            (Some(_), Err(_)) => false,
        }
    }

    /// 两个路径是否指向同一个文件（大小写不敏感；能 canonicalize 时用它消除
    /// 8.3 短路径与 `\\?\` 前缀的形态差）
    fn same_path(left: &Path, right: &Path) -> bool {
        if let (Ok(a), Ok(b)) = (left.canonicalize(), right.canonicalize()) {
            return a == b;
        }
        left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy())
    }

    /// 旧安装目录：`%LOCALAPPDATA%\<产品名>`，连同它对应的产品名
    fn legacy_dirs() -> Vec<(&'static str, PathBuf)> {
        let Some(local) = env_path("LOCALAPPDATA") else {
            return Vec::new();
        };
        PRODUCT_NAMES.iter().map(|name| (*name, local.join(name))).collect()
    }

    /// 读环境变量里的目录（`%LOCALAPPDATA%` / `%APPDATA%` / `%USERPROFILE%`）
    fn env_path(name: &str) -> Option<PathBuf> {
        let value = std::env::var(name).ok()?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    }

    /// 目录里是否有本产品的主程序；没有就认为不是本产品的安装目录
    fn looks_like_our_install(dir: &Path) -> bool {
        BINARY_NAMES.iter().any(|name| dir.join(name).is_file())
    }

    /// 当前进程是否运行在该目录里（开发态或用户把新版拷进旧目录时绝不能删自己）
    fn running_inside(dir: &Path) -> bool {
        let (Ok(exe), Ok(target)) = (std::env::current_exe(), dir.canonicalize()) else {
            return false;
        };
        exe.canonicalize().map(|exe| exe.starts_with(&target)).unwrap_or(false)
    }

    /// 删除一个旧安装目录；返回「该目录是否已不存在」（删除成功或本来就没有）
    fn remove_legacy_dir(dir: &Path) -> bool {
        if !dir.exists() {
            return true;
        }
        if !dir.is_dir() {
            eprintln!("[Cleanup] 跳过旧安装路径（同名但不是目录）: {}", dir.display());
            return false;
        }
        if running_inside(dir) {
            eprintln!("[Cleanup] 跳过旧安装目录（当前程序正在其中运行）: {}", dir.display());
            return false;
        }
        if !looks_like_our_install(dir) {
            eprintln!(
                "[Cleanup] 跳过旧安装目录（未发现本产品主程序，可能不是本产品安装的）: {}",
                dir.display()
            );
            return false;
        }
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                eprintln!(
                    "[Cleanup] 已清理旧用户级安装目录（新版装在 Program Files）: {}",
                    dir.display()
                );
                true
            }
            Err(error) => {
                eprintln!(
                    "[Cleanup] 清理旧安装目录失败（下次启动再试）: {} — {error}",
                    dir.display()
                );
                false
            }
        }
    }

    /// 旧快捷方式：开始菜单 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\<产品名>.lnk`
    /// 与桌面 `%USERPROFILE%\Desktop\<产品名>.lnk`。
    ///
    /// 判定用「文件名 + .lnk 内容里出现旧目录绝对路径」，不解析 .lnk 的目标
    /// （那要走 COM 的 IShellLink，代价大得多）：.lnk 把目标路径以 UTF-16 明文
    /// 存在文件里，两个条件同时成立才删，用户自己建的其它快捷方式不会被误删。
    fn cleanup_shortcuts(dirs: &[(&'static str, PathBuf, bool)]) {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(appdata) = env_path("APPDATA") {
            roots.push(appdata.join("Microsoft\\Windows\\Start Menu\\Programs"));
        }
        if let Some(profile) = env_path("USERPROFILE") {
            roots.push(profile.join("Desktop"));
            // 桌面被重定向到 OneDrive 时（很常见）真实位置在这里；
            // 两个候选都查一遍，反正文件名与内容的双重判定不会误删
            if let Some(onedrive) = env_path("OneDrive") {
                roots.push(onedrive.join("Desktop"));
            }
        }
        for root in roots {
            for (name, dir, removed) in dirs {
                // 旧目录还在（删不掉或不是我们的）：快捷方式是它唯一的入口，留着
                if !removed {
                    continue;
                }
                let shortcut = root.join(format!("{name}.lnk"));
                if !shortcut.is_file() {
                    continue;
                }
                if !points_into(&shortcut, dir) {
                    eprintln!(
                        "[Cleanup] 跳过快捷方式（未指向旧安装目录，可能已被改过）: {}",
                        shortcut.display()
                    );
                    continue;
                }
                match std::fs::remove_file(&shortcut) {
                    Ok(()) => eprintln!("[Cleanup] 已清理旧快捷方式: {}", shortcut.display()),
                    Err(error) => eprintln!(
                        "[Cleanup] 清理旧快捷方式失败（下次启动再试）: {} — {error}",
                        shortcut.display()
                    ),
                }
            }
        }
    }

    /// .lnk 内容里是否出现该目录的绝对路径（UTF-16LE 明文，大小写不敏感）
    fn points_into(shortcut: &Path, dir: &Path) -> bool {
        let Ok(bytes) = std::fs::read(shortcut) else {
            return false;
        };
        let needle: Vec<u8> = dir.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect();
        if needle.is_empty() || bytes.len() < needle.len() {
            return false;
        }
        bytes.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(&needle))
    }

    /// 旧卸载注册项。只有「目录已经不在」时才删：目录还在（删不掉或不是我们的）
    /// 就保留这个入口，用户仍能用它正常卸载。
    fn cleanup_uninstall_keys(dirs: &[(&'static str, PathBuf, bool)]) {
        for (name, dir, removed) in dirs {
            if !removed {
                continue;
            }
            let sub_key = format!("{UNINSTALL_KEY}\\{name}");
            // 键名相同还不够：DisplayName 必须也等于产品名（同名键可能是别的软件）
            let Some(display) = registry::read_string(HKEY_CURRENT_USER, &sub_key, "DisplayName")
            else {
                continue;
            };
            if display.trim() != *name {
                eprintln!("[Cleanup] 跳过卸载注册项（DisplayName 不匹配）: {sub_key} → {display}");
                continue;
            }
            // 键里记录的安装位置也必须落在该旧目录内；读不到就当「没记录」——
            // 键名与 DisplayName 已经双重确认是本产品
            if let Some(recorded) = recorded_install_dir(&sub_key) {
                if !path_within(&recorded, dir) {
                    eprintln!(
                        "[Cleanup] 跳过卸载注册项（记录的安装位置不在旧目录内）: {sub_key} → {}",
                        recorded.display()
                    );
                    continue;
                }
            }
            if registry::delete_key(HKEY_CURRENT_USER, &sub_key) {
                eprintln!("[Cleanup] 已清理旧卸载注册项: {sub_key}");
            } else {
                eprintln!("[Cleanup] 清理旧卸载注册项失败（下次启动再试）: {sub_key}");
            }
        }
    }

    /// 卸载键里记录的安装位置：优先 InstallLocation，其次从 UninstallString 推
    fn recorded_install_dir(sub_key: &str) -> Option<PathBuf> {
        if let Some(text) = registry::read_string(HKEY_CURRENT_USER, sub_key, "InstallLocation") {
            let path = PathBuf::from(trim_quotes(&text));
            if !path.as_os_str().is_empty() {
                return Some(path);
            }
        }
        let command = registry::read_string(HKEY_CURRENT_USER, sub_key, "UninstallString")?;
        exe_path_from(&command)?.parent().map(Path::to_path_buf)
    }

    /// 旧自启登记：值名等于产品名、内容里出现旧目录路径、且它指向的可执行文件
    /// 已经不在了 —— 三条同时成立才删（旧程序还在磁盘上时登记仍然有效）。
    fn cleanup_autostart_entries(dirs: &[(&'static str, PathBuf, bool)]) {
        let paths: Vec<PathBuf> = dirs.iter().map(|(_, dir, _)| dir.clone()).collect();
        for name in PRODUCT_NAMES {
            let Some(value) = registry::read_string(HKEY_CURRENT_USER, RUN_KEY, name) else {
                continue;
            };
            let Some(target) = autostart_target(&value, &paths) else {
                continue;
            };
            if target.is_file() {
                continue;
            }
            // 同时清掉任务管理器「启动」页的状态值，否则启动项列表里会留下一个
            // 指向已删除程序、永远启动不起来的幽灵条目
            let removed = registry::delete_value(HKEY_CURRENT_USER, RUN_KEY, name);
            registry::delete_value(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, name);
            if removed {
                eprintln!("[Cleanup] 已清理旧开机自启登记: {name}");
            }
        }
    }

    /// 自启值里指向旧安装目录的可执行文件路径
    fn autostart_target(value: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
        let exe = exe_path_from(value)?;
        dirs.iter().any(|dir| path_within(&exe, dir)).then_some(exe)
    }

    /// 任务管理器「启动」页是否把这一项停用了。
    ///
    /// 判定与 tauri-plugin-autostart 底层的 auto-launch 一致（该键下同名值的
    /// 末尾 8 字节全为 0 表示启用），这样「我们是否该重新登记」与插件自己的
    /// `is_enabled()` 口径不会打架。值不存在表示从未被单独停用。
    fn startup_disabled(name: &str) -> bool {
        let Some(bytes) = registry::read_binary(HKEY_CURRENT_USER, STARTUP_APPROVED_KEY, name)
        else {
            return false;
        };
        bytes.len() >= 8 && !bytes[bytes.len() - 8..].iter().all(|byte| *byte == 0)
    }

    /// 从命令串里取出可执行文件的绝对路径：以 .exe 结尾（大小写不敏感），
    /// 之后的参数一律丢掉；取不到返回 None（调用方按「拿不到」处理，宁可不删）。
    ///
    /// 用小写化后的串**定位**、在原串上**切片**：这里必须用 `to_ascii_lowercase`
    /// 而不是 `to_lowercase` —— 后者对某些非 ASCII 字符（如 'İ'）会改变字节长度，
    /// 拿它的下标去切原串会切错位置；`to_ascii_lowercase` 只折叠 A-Z，长度不变。
    fn exe_path_from(command: &str) -> Option<PathBuf> {
        let text = trim_quotes(command);
        let end = text.to_ascii_lowercase().find(".exe")? + 4;
        Some(PathBuf::from(&text[..end]))
    }

    /// NSIS 写进注册表的值常带引号（`"C:\...\uninstall.exe"`，也可能在引号后带参数）
    fn trim_quotes(text: &str) -> &str {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix('"') else {
            return trimmed;
        };
        rest.split('"').next().unwrap_or(rest)
    }

    /// 记录的路径是否落在旧目录内（大小写不敏感）。两侧都能 canonicalize 时优先
    /// 用它，消除 8.3 短路径与 `\\?\` 前缀的形态差；目录已不存在时退回字符串比较。
    fn path_within(candidate: &Path, dir: &Path) -> bool {
        if let (Ok(left), Ok(right)) = (candidate.canonicalize(), dir.canonicalize()) {
            return left.starts_with(&right);
        }
        let left = candidate.to_string_lossy().to_lowercase();
        let right = dir.to_string_lossy().to_lowercase();
        left == right || left.starts_with(&format!("{right}\\"))
    }
}

/// 只用到「读字符串值 / 读二进制值 / 删值 / 删键」的最小注册表封装。
///
/// 为什么手写 extern 而不是给 windows-sys 加 Registry 特性：Cargo.toml 在本次
/// 改动的文件域之外（多处并行改动共用它）。这几个 API 签名长期稳定、调用点很少，
/// 按 `login.rs` 里 `ShellExecuteW` 的同一手法声明即可，零新依赖。
#[cfg(windows)]
mod registry {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    /// 预定义根键。值等于 `(LONG)0x80000001` 在 64 位下的符号扩展形态
    /// （与 windows-sys 的 `HKEY_CURRENT_USER` 定义一致，别写成 0x80000001）。
    pub const HKEY_CURRENT_USER: *mut c_void = -2147483647i32 as isize as *mut c_void;

    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const ERROR_SUCCESS: i32 = 0;
    const REG_SZ: u32 = 1;
    const REG_EXPAND_SZ: u32 = 2;
    const REG_BINARY: u32 = 3;

    /// 值长度上限：这里读的都是短字符串，超长说明不是我们要看的值
    const MAX_VALUE_BYTES: u32 = 8 * 1024;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            hkey: *mut c_void,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: *mut c_void,
            value_name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            size: *mut u32,
        ) -> i32;
        fn RegDeleteKeyW(hkey: *mut c_void, sub_key: *const u16) -> i32;
        fn RegDeleteValueW(hkey: *mut c_void, value_name: *const u16) -> i32;
        fn RegCloseKey(hkey: *mut c_void) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 已打开的键：Drop 时关闭，免得每条失败分支都要记得手动关
    struct Key(*mut c_void);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(root: *mut c_void, sub_key: &str, sam: u32) -> Option<Key> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        let name = wide(sub_key);
        let status = unsafe { RegOpenKeyExW(root, name.as_ptr(), 0, sam, &mut handle) };
        if status == ERROR_SUCCESS && !handle.is_null() {
            Some(Key(handle))
        } else {
            None
        }
    }

    /// 读一个值并校验类型；键/值不存在、类型不符、超长都返回 None。
    ///
    /// 缓冲不够时 `RegQueryValueExW` 返回 ERROR_MORE_DATA 并把所需长度写回 size，
    /// 按这个长度重试一次（与 winreg 的 `get_raw_value` 同一套写法：不依赖
    /// 「lpData 传 NULL 先问长度」那条约定，那条在某些值类型上行为不一致）。
    fn query(key: &Key, value: &str, allowed: &[u32]) -> Option<Vec<u8>> {
        const ERROR_MORE_DATA: i32 = 234;

        let name = wide(value);
        let mut kind: u32 = 0;
        // 这里读的都是短字符串，1KB 起步基本一次就读完；超限由下面的判断兜住
        let mut size: u32 = 1024;
        let mut buffer = vec![0u8; size as usize];
        loop {
            let status = unsafe {
                RegQueryValueExW(
                    key.0,
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    &mut kind,
                    buffer.as_mut_ptr(),
                    &mut size,
                )
            };
            if status == ERROR_MORE_DATA {
                // size 已被写成所需长度；超过上限说明不是我们要看的值，放弃
                if size > MAX_VALUE_BYTES {
                    return None;
                }
                buffer = vec![0u8; size as usize];
                continue;
            }
            if status != ERROR_SUCCESS || !allowed.contains(&kind) {
                return None;
            }
            buffer.truncate(size as usize);
            return Some(buffer);
        }
    }

    /// 读字符串值（REG_SZ / REG_EXPAND_SZ）
    pub fn read_string(root: *mut c_void, sub_key: &str, value: &str) -> Option<String> {
        let key = open(root, sub_key, KEY_QUERY_VALUE)?;
        let bytes = query(&key, value, &[REG_SZ, REG_EXPAND_SZ])?;
        let units: Vec<u16> =
            bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }

    /// 读二进制值（任务管理器「启动」页的启用状态用它）
    pub fn read_binary(root: *mut c_void, sub_key: &str, value: &str) -> Option<Vec<u8>> {
        let key = open(root, sub_key, KEY_QUERY_VALUE)?;
        query(&key, value, &[REG_BINARY])
    }

    /// 删键（要求该键没有子键，卸载键都是叶子）
    pub fn delete_key(root: *mut c_void, sub_key: &str) -> bool {
        let name = wide(sub_key);
        unsafe { RegDeleteKeyW(root, name.as_ptr()) == ERROR_SUCCESS }
    }

    /// 删值
    pub fn delete_value(root: *mut c_void, sub_key: &str, value: &str) -> bool {
        let Some(key) = open(root, sub_key, KEY_SET_VALUE) else {
            return false;
        };
        let name = wide(value);
        unsafe { RegDeleteValueW(key.0, name.as_ptr()) == ERROR_SUCCESS }
    }
}
