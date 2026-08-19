import { install } from './lib/install.js'

export const name = 'dsh-launcher'

const HINT =
  '[dsh-launcher] Desktop shortcut ready - next time just double-click "DSH Launcher" on your desktop.\n' +
  '[dsh-launcher] It points straight at the bundled exe (node_modules/dsh-launcher/dist), nothing is copied.\n' +
  '[dsh-launcher] (pnpm >=10 blocks install-time scripts, so this init runs once on the first dsh start after add.)\n' +
  '[dsh-launcher] 桌面快捷方式已就绪,之后双击桌面 "DSH Launcher" 即可进入,直接指向随包 exe(无复制)。\n' +
  '[dsh-launcher] (pnpm ≥10 安全限制禁止安装时执行脚本,因此初始化在 add 后首次启动 dsh 时完成;之后不再重复创建。)'

export function apply() {
  install().then(
    (result) => {
      console.log(`[dsh-launcher] ${result.message}`)
      if (result.created) {
        console.log(HINT)
      }
    },
    (error) => console.warn(`[dsh-launcher] ${error.message}`),
  )
}