# 编译、运行网页视图的例子（Windows）：
#
#     .\crates\webview\run-example.ps1 browser
#     .\crates\webview\run-example.ps1 browser -Release https://github.com --quit-after 10
#     .\crates\webview\run-example.ps1 <编成动态库的例子> -Release -Sandbox
#
# 要先装好：Rust、cmake、Ninja、Visual Studio 的"使用 C++ 的桌面开发"（编 CEF 的 C++ 包装层）。
# 第一次编译时自动下载 CEF 内核（几百 MB）；设了 CEF_PATH 时下载到那里，换个目录重新编译不用再下。
#
# Windows 上 CEF 的运行文件（libcef.dll、资源、语言包）由编译脚本拷到目标目录（target\debug 或 target\release），
# 子进程程序 aureole-webview-helper.exe 也编在那里；例子编在 examples 子目录里，这里拷到它们旁边再运行。
# -Sandbox：例子是编成动态库的沙箱版（导出 RunWinMain），把它和改名成同名的 bootstrap.exe（CEF 带的，也在目标目录里）
# 放到目标目录里运行，网页的子进程都在沙箱里。
# 选项：-Release 编发布版；-NoRun 只编译不运行；后面的参数交给例子（比如网址、--quit-after 10）。
param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Name,
    [switch]$Release,
    [switch]$NoRun,
    [switch]$Sandbox,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest
)
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$build = if ($Release) { 'release' } else { 'debug' }
$cargoArgs = @('build', '--example', $Name)
if (-not $Sandbox) { $cargoArgs += @('--bin', 'aureole-webview-helper') }
if ($Release) { $cargoArgs += '--release' }

Push-Location $here
try {
    cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}

$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $here 'target' }
$target = Join-Path $targetRoot $build
$exe = Join-Path $target "$Name.exe"
# 运行中的程序换不掉：先关掉上一次开着的例子
if ($Sandbox) {
    # bootstrap.exe 改成例子的名字，加载旁边同名的 .dll
    Copy-Item (Join-Path $target "examples\$Name.dll") (Join-Path $target "$Name.dll") -Force
    Copy-Item (Join-Path $target 'bootstrap.exe') $exe -Force
} else {
    Copy-Item (Join-Path $target "examples\$Name.exe") $exe -Force
}
Write-Host "编好了：$exe"

if (-not $NoRun) {
    $exampleArgs = @($Rest | Where-Object { $_ -ne '--' })
    & $exe @exampleArgs
    exit $LASTEXITCODE
}
