#!/bin/bash
# 编译、打包、运行网页视图的例子（macOS）：
#
#     CEF_PATH=…/target/cef crates/webview/run-example.sh browser
#     CEF_PATH=…/target/cef crates/webview/run-example.sh browser -- https://example.org --quit-after 10
#     CEF_PATH=…/target/cef crates/webview/run-example.sh browser --release --no-run
#
# macOS 上 CEF 要求程序在应用包里：内核框架在 Contents/Frameworks 下，旁边是 5 个辅助程序的包
# （"<例子> Helper.app"，还有 GPU、Renderer、Plugin、Alerts 四种），它们的可执行文件都是 aureole-webview-helper。
# 这个脚本编好例子和 aureole-webview-helper，在目标目录里拼出 examples/<例子>.app（内核框架从 CEF_PATH 同步过去，
# 只有第一次要拷几百 MB，语言包只留英文和中文），然后在前台运行它，日志打在终端里，关窗口或 Ctrl+C 退出。
#
# 选项：--release 编发布版；--no-run 只打包不运行；-- 后面的参数交给例子。
# CEF_PATH 是 CEF 内核下载到的目录（和编译时用的同一个），没设时用仓库根目录下的 target/cef。
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
if [[ "$(uname)" != "Darwin" ]]; then
    echo "这个脚本只管 macOS。Windows 上用 crates/webview/run-example.ps1；Linux 上把 aureole-webview-helper 和 CEF 的运行文件放在例子的可执行文件旁边再运行" >&2
    exit 1
fi

name=""
profile="debug"
cargo_profile=()
run=1
args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) profile="release"; cargo_profile=(--release) ;;
        --no-run) run=0 ;;
        --) shift; args=("$@"); break ;;
        -*) echo "不认识的选项 $1" >&2; exit 1 ;;
        *) name="$1" ;;
    esac
    shift
done
if [[ -z "$name" ]]; then
    echo "用法：$0 <例子> [--release] [--no-run] [-- 例子的参数…]" >&2
    exit 1
fi

export CEF_PATH="${CEF_PATH:-$here/../../target/cef}"
if [[ ! -d "$CEF_PATH" ]]; then
    echo "找不到 CEF 内核的目录 $CEF_PATH（设 CEF_PATH）" >&2
    exit 1
fi
framework="$(find "$CEF_PATH" -maxdepth 3 -type d -name "Chromium Embedded Framework.framework" -print -quit)"
if [[ -z "$framework" ]]; then
    echo "$CEF_PATH 里没有 Chromium Embedded Framework.framework（先编译一次，让它下载内核）" >&2
    exit 1
fi

cd "$here"
cargo build ${cargo_profile[@]+"${cargo_profile[@]}"} --example "$name" --bin aureole-webview-helper
target="${CARGO_TARGET_DIR:-$here/target}/$profile"
app="$target/examples/$name.app"
identifier="com.example.aureole-webview.${name//_/-}"

# Info.plist：$1 包的目录，$2 可执行文件名，$3 包标识符，$4 是辅助程序时为 1（不在程序坞里显示）
write_plist() {
    local ui_element=""
    if [[ "$4" == 1 ]]; then
        ui_element="<key>LSUIElement</key><true/>"
    fi
    cat > "$1/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>$2</string>
    <key>CFBundleIdentifier</key><string>$3</string>
    <key>CFBundleName</key><string>$2</string>
    <key>CFBundleDisplayName</key><string>$2</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleVersion</key><string>0.0.1</string>
    <key>CFBundleShortVersionString</key><string>0.0.1</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSEnvironment</key><dict><key>MallocNanoZone</key><string>0</string></dict>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    $ui_element
</dict>
</plist>
EOF
}

# 可执行文件换成新的：先删再拷（原地覆盖已经运行过的程序，系统会按旧的签名缓存把它杀掉）
install_executable() {
    rm -f "$2"
    cp "$1" "$2"
}

mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"
install_executable "$target/examples/$name" "$app/Contents/MacOS/$name"
write_plist "$app" "$name" "$identifier" 0
# 只放要的语言包：内核带着两百多种（约 50 MB），默认只留英文和中文。
# AUREOLE_WEBVIEW_LOCALES 换成别的（空格隔开的 .lproj 名字，比如 "en zh_CN ja"）
locales=()
for locale in ${AUREOLE_WEBVIEW_LOCALES:-en zh_CN zh_TW}; do
    locales+=(--include="Resources/$locale.lproj/***")
done
rsync -a --delete --delete-excluded "${locales[@]}" --exclude='Resources/*.lproj' \
    "$framework/" "$app/Contents/Frameworks/Chromium Embedded Framework.framework/"
for kind in "" " (GPU)" " (Renderer)" " (Plugin)" " (Alerts)"; do
    helper="$name Helper$kind"
    bundle="$app/Contents/Frameworks/$helper.app"
    suffix="$(echo "$kind" | tr -d ' ()' | tr '[:upper:]' '[:lower:]')"
    mkdir -p "$bundle/Contents/MacOS"
    install_executable "$target/aureole-webview-helper" "$bundle/Contents/MacOS/$helper"
    write_plist "$bundle" "$helper" "$identifier.helper${suffix:+.$suffix}" 1
done
echo "打包好了：$app"

if [[ "$run" == 1 ]]; then
    exec "$app/Contents/MacOS/$name" ${args[@]+"${args[@]}"}
fi
