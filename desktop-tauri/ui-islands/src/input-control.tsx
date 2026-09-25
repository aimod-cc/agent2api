import * as React from 'react'
import { Input } from '@base-ui/react/input'

/**
 * 搜索 / 文本输入框。
 *
 * 用 Base UI 的 Input（就是原生 `<input>` 的薄封装，多了 `data-*` 状态属性，
 * 将来要接 Field 做校验时不必换组件）；外观完全走 ui/css 里既有的
 * `input[type=…]` 与 `.input-affix` 规则，不引入第二套观感。
 *
 * **非受控**，这是与分段控件（完全受控）有意的不同：搜索框每次按键都变，
 * 若走受控，一次输入要等 React 把状态 flush 完才上屏 —— 而 onInput 里还要
 * 重绘整张模型表，慢的时候打字会明显发涩。非受控下浏览器先把字上屏，
 * 外部随后的重绘再慢也不影响手感。外部要改值（启动回填、清空）走岛的
 * `setValue`，由它命令式写 DOM。
 */

type InputControlProps = {
  /** 初始值（非受控，之后由浏览器持有） */
  defaultValue?: string
  placeholder?: string
  /** 原生 input 类型，默认 search（带原生清除按钮） */
  type?: string
  'aria-label': string
  /** 前缀图标字符（如 ⌕）；不传就不留图标位 */
  icon?: string
  onInput?: (value: string) => void
  /** 岛靠它拿到真实 DOM，用于命令式设值与聚焦 */
  domRef: { current: HTMLInputElement | null }
}

function InputControl({
  defaultValue,
  placeholder,
  type = 'search',
  icon,
  onInput,
  domRef,
  ...props
}: InputControlProps) {
  return (
    <span className='input-affix'>
      {icon ? <span className='affix'>{icon}</span> : null}
      <Input
        ref={node => {
          domRef.current = node as HTMLInputElement | null
        }}
        type={type}
        defaultValue={defaultValue}
        placeholder={placeholder}
        autoComplete='off'
        onInput={event => onInput?.(event.currentTarget.value)}
        {...props}
      />
    </span>
  )
}

export { InputControl, type InputControlProps }
