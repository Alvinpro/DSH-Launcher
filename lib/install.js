import { execFileSync } from 'node:child_process'
import { stat, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

/** 包内随附的 exe(打进仓库,add 时随包进入 node_modules,无需复制/下载)。 */
export const BUNDLED_EXE = fileURLToPath(new URL('../dist/dsh-launcher.exe', import.meta.url))
export const SHORTCUT_NAME = 'DSH Launcher.lnk'
/** 首次创建后写入的标记:存在即表示快捷方式已生成过,之后不再重复创建(用户删除快捷方式也不会重建)。 */
const MARKER = fileURLToPath(new URL('../.shortcut-created', import.meta.url))
const MIN_SIZE = 50 * 1024

function createShortcut() {
  const script = [
    `$target = '${BUNDLED_EXE}'`,
    `$desktop = [Environment]::GetFolderPath('Desktop')`,
    `if (-not $desktop) { $desktop = Join-Path $env:USERPROFILE 'Desktop' }`,
    `$lnk = Join-Path $desktop '${SHORTCUT_NAME}'`,
    `$w = New-Object -ComObject WScript.Shell`,
    `$s = $w.CreateShortcut($lnk)`,
    `$s.TargetPath = $target`,
    `$s.WorkingDirectory = Split-Path $target`,
    `$s.IconLocation = "$target,0"`,
    `$s.Description = 'DSH Launcher - DeepSeek Harness web UI'`,
    `$s.Save()`,
    `Write-Output $lnk`,
  ].join('; ')
  return execFileSync(
    'powershell',
    ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', script],
    { encoding: 'utf8' },
  ).trim()
}

export async function install() {
  if (process.platform !== 'win32') {
    return { message: `non-Windows platform (${process.platform}), skipped / 非 Windows 平台,跳过`, created: false }
  }
  try {
    const st = await stat(BUNDLED_EXE)
    if (st.size < MIN_SIZE) {
      throw new Error(`size ${st.size} < ${MIN_SIZE}`)
    }
  } catch (error) {
    return { message: `bundled exe missing: ${BUNDLED_EXE} (${error.message}) / 包内 exe 缺失`, created: false }
  }
  // 标记已存在 → 快捷方式已在首次运行时创建过,之后不再重复创建(即使快捷方式被删除也不重建)
  try {
    await stat(MARKER)
    return { message: 'shortcut was created on first run, skip / 快捷方式已在首次运行时创建,跳过', created: false }
  } catch {
    // marker 不存在 → 首次运行,继续创建
  }
  const lnk = await createShortcut()
  try {
    await writeFile(MARKER, `created at ${new Date().toISOString()} by dsh-launcher plugin\n`, 'utf8')
  } catch {
    // 包目录不可写时忽略标记写入:下次启动会重试一次(仅此副作用)
  }
  return { message: `desktop shortcut ready: ${lnk} -> ${BUNDLED_EXE} / 桌面快捷方式已就绪,直接指向随包 exe(无需复制)`, created: true }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const result = await install()
  console.log(`[dsh-launcher] ${result.message}`)
}