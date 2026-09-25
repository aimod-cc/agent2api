import { mountInput } from './input'
import { mountSegmented } from './segmented'

/**
 * 岛的统一入口。
 *
 * 两个岛打进同一个 bundle 是刻意的：React 与 react-dom 只能有一份实例
 * （各打各的会把 React 装两遍，体积翻倍，而且两边的调度器互不认识）。
 * 于是 index.html 里也只有一个 <script>，所有岛共用它。
 *
 * 挂载点约定：宿主元素用 `.island`（display: contents，本身不生成盒子），
 * 岛渲染出来的根元素直接参与父级布局，接入前后布局等价。
 */
declare global {
  interface Window {
    /** 分段筛选：见 segmented.tsx */
    wbSegmented?: { mount: typeof mountSegmented }
    /** 搜索 / 文本输入框：见 input.tsx */
    wbInput?: { mount: typeof mountInput }
  }
}

window.wbSegmented = { mount: mountSegmented }
window.wbInput = { mount: mountInput }
