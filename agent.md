# Agent.MD — 发版流程

> 本文档面向维护者与 AI 代理：Agent2API（workbuddy）桌面端 + Docker 镜像的**标准发版流程**。
> 所有发布动作都由 `.github/workflows/` 下的三个工作流自动完成，人工只负责「提交、打 tag、挂 GitHub Release、验收」。

## 0. 版本号约定

- 版本号形如 `X.Y.Z`（如 `2.7.2`），日常小版本「递增 0.01」= 末位 +1。
- **版本号要同步改 5 处**（缺一处会导致安装包与显示版本对不上）：
  1. `package.json`（根）
  2. `desktop-tauri/package.json`
  3. `desktop-tauri/src-tauri/Cargo.toml`
  4. `desktop-tauri/src-tauri/tauri.conf.json`
  5. `desktop-tauri/src-tauri/server/Cargo.toml`
- 应用内「关于」与更新检查读的是 `env!("CARGO_PKG_VERSION")`（第 5 处），改完跑一次 `cargo check` 让 `Cargo.lock` 跟着刷新。

## 1. 提交：保证工作区干净

发版前必须把工作区收干净，tag 里的代码就是发出去的代码：

```bash
git status --short        # 必须为空；有改动就先提交或清理
cargo check               # 或前端有改动时做一次构建自检
```

## 2. 发布提交：更新日志写进提交信息

**发布提交的提交信息 = 更新日志**。原因：`gitee-release` 工作流创建 Gitee 发行版时，正文自动取「tag 所指提交的提交信息」（`git log -1 --format=%B`）。

- 格式沿用历史版本的编号清单，每行一条，带分类前缀：`新增：` / `优化：` / `修复：` / `更新：` / `移除：`，最后一条固定 `版本：X.Y.Z → X.Y.Z+1`；
- 内容取「上个 tag 以来的全部提交」综合整理（`git log vX.Y.Z..HEAD --oneline`），纯 CI/临时的调试提交归并成一条流程性描述即可；
- 本次没有代码改动时用空提交承载：`git commit --allow-empty -F 更新日志.txt`（历史发版两种做法都有先例）。

## 3. 打 tag 并推送：一条 tag 触发全部构建

```bash
git push origin main
git tag vX.Y.Z
git push origin vX.Y.Z
```

推 `v*` tag 会**同时触发三个工作流**（`.github/workflows/`）：

| 工作流 | 产出 | 说明 |
|---|---|---|
| `build.yml`（build） | Windows NSIS 安装包 + macOS universal dmg + **Gitee 发行版** | `macos` / `windows` 两个 job 构建并上传 artifact（macOS 包**只能在 CI 构建**，无法从 Windows 交叉编译）；`gitee-release` job 自动在 Gitee 创建同名发行版（正文取 tag 提交信息）并上传两个安装包 |
| `docker.yml`（docker） | Docker Hub `aimodcc/agent2api:<版本>` + `:latest`（amd64 / arm64 双架构） | 手动 `workflow_dispatch` 触发时只出 `:dev` 测试 tag，不碰正式 tag |
| `sync-gitee.yml`（sync-gitee） | Gitee 仓库镜像同步 | main 与 `v*` tag 推送后强制同步到 `gitee.com/aimodcc/agent2api` |

跟踪进度：

```bash
gh run list --limit 4          # 确认三个工作流都已触发
gh run watch <run-id> --exit-status
```

## 4. 挂 GitHub Release（人工一步）

GitHub Release **不会自动创建**，由本地 `gh` 挂载：

```bash
gh run download <build-run-id> -D dist/     # 下载 macos-universal / windows-nsis 两个产物
gh release create vX.Y.Z --title vX.Y.Z \
  --notes-file 更新日志.txt \
  dist/windows-nsis/*_x64-setup.exe dist/macos-universal/*_universal.dmg
```

Gitee 侧的发行版则是全自动的，无需人工操作。

## 5. 验收清单（四端核对）

- [ ] GitHub Release：`gh release view vX.Y.Z` —— 正文 19+N 条日志齐全，exe / dmg 两个附件都在；
- [ ] Docker Hub：`aimodcc/agent2api` 的 Tags 页出现 `<版本>` 与 `latest`，Pushed 时间一致；
- [ ] Gitee 发行版：`https://gitee.com/aimodcc/agent2api/releases/tag/vX.Y.Z` —— 正文完整、exe / dmg 两个附件都在；
- [ ] 安装包「关于」页版本号与 tag 一致。

## 6. 已知坑与排查

- **gitee-release 上传跨境假死**：GitHub 托管 runner（境外）直连 Gitee（境内）传附件会**静默挂死**（v2.7.2 实测 11MB 挂 10 分钟+，curl 无超时的话会挂到 6 小时 job 超时）。已加三层兜底：连接 20s 超时 / 传输中 30 秒低于 1KB/s 判假死 / 单次 10 分钟硬上限，失败自动重试 3 次（见 `build.yml` 的 `CURL` 变量说明）。若 job 最终失败，`gh run rerun <run-id> --failed` 重跑即可——该 job 幂等（先删同名发行版再重建）。
- **Gitee 附件 ≤100MB**：当前两个安装包在几 MB 量级，暂时够用；超过就换国内对象存储。
- **更新源依赖 Gitee 附件齐全**：应用内「软件更新」支持 GitHub / Gitee 双源，Gitee 源读 `releases/latest`——所以 Gitee 发行版的附件必须传全，否则选了 Gitee 源的用户更新不到。
- **GHCR 新包默认私有**：若以后镜像改推 GHCR，首次推送后需到包设置手动改 Public（当前推的是 Docker Hub，无此问题）。
