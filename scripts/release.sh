#!/usr/bin/env bash
# 发版收尾脚本：v* tag 的 build 工作流跑完后，一条命令挂全两个发行版。
#
#   bash scripts/release.sh vX.Y.Z            # 自动找该 tag 的成功 build run
#   bash scripts/release.sh vX.Y.Z <run-id>   # 或显式指定 run
#
# 做四件事：
#   1. 下载 build 工作流的两个安装包 artifact（windows-nsis / macos-universal）到 dist/；
#   2. 用 tag 所指提交的提交信息（= 更新日志，见 AGENT.md 第 2 节）创建 / 更新
#      GitHub Release 并挂两个附件；
#   3. 在 Gitee 创建同名发行版（正文同源）并上传两个附件；
#   4. 打印验收提示。
#
# ── 为什么 Gitee 上传放在本地 ─────────────────────────────────
# GitHub 托管 runner（境外）直连 Gitee（境内）传附件慢且会**假死**：
# v2.7.2 实测 11MB 挂 10 分钟+，v2.7.3 的 gitee-release job 又实测
# dmg 上传 10 分钟 0 字节超时。v2.7.3 起撤掉 CI 跨境上传，本脚本在本地
# 境内直连 Gitee，秒传。本机到 GitHub 的下载走系统代理，量级几 MB。
#
# 幂等可重跑：GitHub 侧附件 --clobber 覆盖；Gitee 侧先删同名发行版再重建。
#
# 令牌：优先读环境变量 GITEE_TOKEN，其次读 ~/.gitee-token 文件（一行纯
# token，chmod 600，已被 .gitignore 挡住勿提交）。token 在 Gitee
# 「设置 → 私人令牌」，需要 releases 读写权限。

set -euo pipefail

TAG="${1:?用法: scripts/release.sh vX.Y.Z [build-run-id]}"
RUN_ID="${2:-}"

case "$TAG" in
  v*) ;;
  *) echo "错误: '$TAG' 不是 v* 形式的 tag" >&2; exit 1 ;;
esac

command -v gh   >/dev/null || { echo "错误: 未安装 gh CLI" >&2; exit 1; }
command -v curl >/dev/null || { echo "错误: 未安装 curl" >&2; exit 1; }
# Gitee API 返回的 JSON 用 node 解析（不引入 jq 依赖，两端都有 node）
command -v node >/dev/null || { echo "错误: 未安装 node" >&2; exit 1; }

# tr -d 兜底：记事本 / PowerShell 重定向写出来的文件可能带 \r\n 或 BOM，会毁掉认证头
GITEE_TOKEN="${GITEE_TOKEN:-$(tr -d '\r\n' < "$HOME/.gitee-token" 2>/dev/null || true)}"
[ -n "$GITEE_TOKEN" ] || {
  echo "错误: 缺少 Gitee 访问令牌 —— export GITEE_TOKEN=xxx，或把 token 写入 ~/.gitee-token" >&2
  exit 1
}

# 从 stdin 的 JSON 里取发行版 id：数组取首个，对象直取；空数组 / 无效 JSON 返回空串。
# （Gitee 按 tag 查发行版在「无发行版」时也返回 200 + 空数组，须解析出真实 id）
release_id() {
  node -e 'let s="";process.stdin.on("data",d=>s+=d);process.stdin.on("end",()=>{try{const j=JSON.parse(s);const id=Array.isArray(j)?(j[0]&&j[0].id):(j&&j.id);process.stdout.write(id==null?"":String(id))}catch{process.stdout.write("")}})'
}

# ── 1. 定位 build run 并下载安装包 ────────────────────────────
if [ -z "$RUN_ID" ]; then
  echo "→ 查找 $TAG 触发的 build 工作流…"
  # 不能按 run 整体 conclusion==success 过滤：tag 触发的构建里
  # gitee-release 失败会把整个 run 拖成 failure/cancelled，而安装包
  # artifact 是好的 —— 所以逐个候选验「两个安装包 artifact 是否齐全」
  for id in $(gh run list --workflow=build --limit 30 --json databaseId,headBranch \
      | node -e 'let s="";process.stdin.on("data",d=>s+=d);process.stdin.on("end",()=>{JSON.parse(s).filter(r=>r.headBranch===process.argv[1]).forEach(r=>process.stdout.write(r.databaseId+"\n"))})' "$TAG"); do
    if gh api "repos/$(gh repo view --json nameWithOwner -q .nameWithOwner)/actions/runs/$id/artifacts" \
         --jq 'any(.artifacts[].name; . == "macos-universal") and any(.artifacts[].name; . == "windows-nsis")' >/dev/null 2>&1; then
      RUN_ID=$id
      break
    fi
  done
  [ -n "$RUN_ID" ] || {
    echo "错误: 没找到 $TAG 的可用 build run（需 macos-universal 与 windows-nsis 两个 artifact 齐全）—— 先确认 CI 跑完（gh run list），或手动传 run-id" >&2
    exit 1
  }
fi
echo "→ build run: $RUN_ID"

rm -rf dist
gh run download "$RUN_ID" -D dist
EXE=$(ls dist/windows-nsis/*.exe)
DMG=$(ls dist/macos-universal/*.dmg)
echo "→ 已下载 $(basename "$EXE") / $(basename "$DMG")"

# ── 2. 更新日志 = tag 所指提交的提交信息（GitHub / Gitee 同源）──
NOTES_FILE=$(mktemp)
trap 'rm -f "$NOTES_FILE"' EXIT
git log -1 --format=%B "$TAG" > "$NOTES_FILE"

echo "→ GitHub Release"
if gh release view "$TAG" >/dev/null 2>&1; then
  gh release upload "$TAG" --clobber "$EXE" "$DMG"
  echo "  ↳ 已存在，附件覆盖上传"
else
  gh release create "$TAG" --title "$TAG" --notes-file "$NOTES_FILE" "$EXE" "$DMG"
fi

# ── 3. Gitee 发行版 ───────────────────────────────────────────
# 本地直连 Gitee 很快，只留轻量保险：连接 20s、单次 300s、重试 2 次
API="https://gitee.com/api/v5/repos/aimodcc/agent2api"
AUTH="Authorization: Bearer ${GITEE_TOKEN}"
GCURL=(curl -sS --connect-timeout 20 --max-time 300 --retry 2 --retry-delay 3)

echo "→ Gitee 发行版"
OLD_ID=$("${GCURL[@]}" -H "$AUTH" "$API/releases/tags/$TAG" | release_id)
if [ -n "$OLD_ID" ]; then
  echo "  ↳ 已存在发行版 id=$OLD_ID，先删除（幂等重建）"
  "${GCURL[@]}" -f -X DELETE -H "$AUTH" "$API/releases/$OLD_ID" >/dev/null
fi

# 三个坑都是实测踩出来的：① body / target_commitish 必填（文档写可选）；
# ② 本机直发 application/json 的 POST 会被 Gitee 网关拦成 HTML 400，
#    表单编码能正常进 API 层（CI 里 JSON 能通，不赌它，统一走表单）；
# ③ 正文必须用 body@文件 让 curl 自己读文件原始字节 —— 内容放进参数
#    （$(cat) 展开）会在 Git Bash → 原生 exe 的 argv 转换中把 UTF-8 按
#    本地代码页转坏（v2.7.3 实测 Gitee 正文全变 U+FFFD）；而 body@file
#    的路径又必须是 Windows 形式（curl 读不了 /tmp 这种 MSYS 虚拟路径），
#    所以经 cygpath -w 转换后传给 curl
SHA=$(git rev-parse "$TAG^{commit}")
NOTES_WIN_PATH=$(cygpath -w "$NOTES_FILE")
RELEASE_ID=$("${GCURL[@]}" -f -X POST -H "$AUTH" \
  --data-urlencode "tag_name=$TAG" \
  --data-urlencode "name=$TAG" \
  --data-urlencode "target_commitish=$SHA" \
  --data-urlencode "body@$NOTES_WIN_PATH" \
  "$API/releases" | release_id)
[ -n "$RELEASE_ID" ] || { echo "错误: Gitee 发行版创建失败" >&2; exit 1; }
echo "  ↳ 已创建 id=$RELEASE_ID"

for f in "$EXE" "$DMG"; do
  echo "  ↳ 上传 $(basename "$f")"
  "${GCURL[@]}" -f -X POST -H "$AUTH" -F "file=@$f" \
    "$API/releases/$RELEASE_ID/attach_files" >/dev/null
  echo "    $(basename "$f") 完成"
done

# ── 4. 验收提示 ───────────────────────────────────────────────
echo
echo "发版收尾完成。四端核对："
echo "  [1] GitHub Release:  gh release view $TAG"
echo "  [2] Docker Hub:      https://hub.docker.com/r/aimodcc/agent2api/tags （$TAG 版本号 + latest）"
echo "  [3] Gitee 发行版:    https://gitee.com/aimodcc/agent2api/releases/tag/$TAG"
echo "  [4] 安装包「关于」页版本号与 $TAG 一致"
