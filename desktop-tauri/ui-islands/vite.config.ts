import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

/**
 * React 岛构建配置。
 *
 * 产物直接落到 `ui/islands/`，由 index.html 以普通 <script> 引入 ——
 * Tauri 打包的就是 `ui/` 这个静态目录（见 tauri.conf.json 的 frontendDist），
 * 所以岛上线的形态必须是「已构建好的 js 文件」，而不是被 Tauri 现编译的源码。
 *
 * 用 iife 而非 esm：现有 60 多个前端脚本都是同步经典脚本、靠加载顺序定依赖，
 * module 脚本是 defer 执行的，混进来会多出一层时序问题。
 */
export default defineConfig({
  plugins: [react()],
  /**
   * lib 模式下 Vite 不替换 `process.env.NODE_ENV`（它假设库产物还要被消费方
   * 再加工），但我们的产物是直接丢给浏览器跑的成品 —— React 内部会读这个值，
   * 不替换就会在运行时抛 `process is not defined`、整个岛静默不挂载。
   */
  define: {
    'process.env.NODE_ENV': JSON.stringify('production'),
  },
  build: {
    outDir: '../ui/islands',
    // 产物目录在项目根之外，且以后会有多个岛各自输出文件，别互相清空
    emptyOutDir: false,
    lib: {
      entry: 'src/index.tsx',
      name: 'WbIslands',
      formats: ['iife'],
      fileName: () => 'ui.js',
    },
    // 产物要提交进仓库，压掉体积；需要调试时把 sourcemap 打开临时构建一次
    minify: 'esbuild',
    sourcemap: false,
    target: 'es2020',
  },
})
