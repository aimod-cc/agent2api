import { createRoot } from 'react-dom/client'

import { InputControl } from './input-control'

/**
 * 输入框岛 —— 搜索框这类「用户直接打字」的控件走这里。
 *
 * 用法（业务侧）：
 *   const island = window.wbInput.mount(host, {
 *     value: saved,                 // 初始值
 *     placeholder: '搜索…',
 *     ariaLabel: '搜索模型',
 *     icon: '⌕',
 *     onInput: value => { ... },    // 每次输入
 *   })
 *   island.setValue('')             // 命令式设值（清空 / 回填）
 *
 * 与分段岛一样是「业务持有状态、岛只渲染」：这里额外说明一点 —— 输入框是
 * **非受控**的，值由浏览器持有，所以 setValue 是命令式写 DOM 而不是灌 props。
 * 理由见 input-control.tsx 顶部（受控会让打字等 React 的 flush，重绘慢时发涩）。
 */

type InputIslandOptions = {
  /** 初始值 */
  value?: string
  placeholder?: string
  /** 原生 input 类型，默认 search */
  type?: string
  /** 无障碍名。搜索框只有 placeholder 时读屏播报偏弱，建议给一个 */
  ariaLabel: string
  /** 前缀图标字符（如 ⌕） */
  icon?: string
  onInput?: (value: string) => void
}

function mountInput(host: HTMLElement, options: InputIslandOptions) {
  const root = createRoot(host)
  /** React 提交后才填得上；mount 返回时若立刻调 setValue 可能还没就绪 */
  const domRef: { current: HTMLInputElement | null } = { current: null }

  root.render(
    <InputControl
      domRef={domRef}
      defaultValue={options.value}
      placeholder={options.placeholder}
      type={options.type}
      icon={options.icon}
      aria-label={options.ariaLabel}
      onInput={options.onInput}
    />
  )

  return {
    /** 命令式设值（清空 / 回填）。非受控组件只能这样改值，改完不触发 onInput。 */
    setValue(value: string) {
      const element = domRef.current
      if (element && element.value !== value) element.value = value
    },
    /** 当前值（外部懒得自己记时用；正常场景业务侧本就有那份状态） */
    getValue() {
      return domRef.current?.value ?? ''
    },
    focus() {
      domRef.current?.focus()
    },
    unmount() {
      root.unmount()
    },
  }
}

type InputIsland = ReturnType<typeof mountInput>

export { mountInput, type InputIsland, type InputIslandOptions }
