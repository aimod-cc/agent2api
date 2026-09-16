#!/usr/bin/env node
/**
 * 构建脚本 — 把网关后端打包成 Tauri 应用可直接运行的形态
 *
 * 产物（都在 desktop-tauri/resources/ 下）：
 *   server.cjs   后端 bundle（server.mjs + src/* + undici + yaml 合成一个文件）
 *   node.exe     官方 Node 运行时（从本机 Node 安装目录原样复制）
 *
 * 为什么用「官方 node.exe + bundle」而不是单文件可执行：
 *   - Bun 编译：Bun 把 import 'undici' 解析成自带 shim，缺 Socks5ProxyAgent，
 *     账号走 socks5 出网会崩；真实 undici 在 Bun 下也加载不了（webidl 不兼容）。
 *   - Node SEA：需要把 blob 注入 node.exe，注入后二进制签名失效，
 *     杀毒软件会直接报「发现可疑程序，存在安全风险」，分发给别人同样会被拦。
 *   官方 node.exe 未做任何改动，签名完整，不会被误报；代价只是安装包大一些。
 *
 * 用法：
 *   node build/build-backend.mjs
 */

import { copyFileSync, existsSync, mkdirSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
// 放在 src-tauri 内：Tauri 的资源路径相对 src-tauri 解析，
// 目录结构会原样保留（→ 运行时 resource_dir()/resources/server.cjs）
const RES_DIR = join(ROOT, 'desktop-tauri', 'src-tauri', 'resources');
const SERVER_BUNDLE = join(RES_DIR, 'server.cjs');
const NODE_BIN = process.platform === 'win32' ? 'node.exe' : 'node';

function log(step, message) {
  console.log(`[build] ${step} ${message}`);
}

/** 取本地依赖的 CLI 入口，用 node 直接执行（Windows 下 .cmd 不能直接 spawn） */
function localCli(pkg, relative) {
  const full = join(ROOT, 'node_modules', pkg, relative);
  if (!existsSync(full)) throw new Error(`找不到 ${pkg}，请先执行 npm install`);
  return full;
}

function mb(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function main() {
  process.chdir(ROOT);
  mkdirSync(RES_DIR, { recursive: true });

  // ── 1. bundle 后端 ────────────────────────────────────────
  // CJS 格式：SEA/直接 node 执行都兼容，且不需要 package.json 声明 type
  execFileSync('node', [
    localCli('esbuild', 'bin/esbuild'),
    join(ROOT, 'server.mjs'),
    '--bundle',
    '--platform=node',
    '--format=cjs',
    '--target=node20',
    '--outfile=' + SERVER_BUNDLE,
    '--log-level=error',
  ], { stdio: ['ignore', 'pipe', 'inherit'] });
  log('1/2', `后端已打包 → server.cjs（${mb(statSync(SERVER_BUNDLE).size)}）`);

  // ── 2. 复制官方 Node 运行时 ───────────────────────────────
  const target = join(RES_DIR, NODE_BIN);
  if (process.platform === 'win32' && process.execPath.toLowerCase().endsWith('node.exe')) {
    copyFileSync(process.execPath, target);
    log('2/2', `运行时已就绪 → ${NODE_BIN}（${mb(statSync(target).size)}，官方原版）`);
  } else if (existsSync(target)) {
    log('2/2', `运行时已存在 → ${NODE_BIN}（非 Windows，跳过复制）`);
  } else {
    throw new Error('未找到 Node 运行时，请手动把 node.exe 放到 desktop-tauri/resources/');
  }

  // ── 自检：bundle 必须能被 node 正常加载 ────────────────────
  const help = execFileSync(process.execPath, [SERVER_BUNDLE, '--help'], { encoding: 'utf8' });
  if (!/WorkBuddy Local Proxy/.test(help)) {
    throw new Error('自检失败：后端 bundle 无法正常启动');
  }
  log('✓', '自检通过，后端资源可用于 Tauri 打包');
}

try {
  main();
} catch (error) {
  console.error('\n❌ 构建失败:', error.message);
  process.exit(1);
}
