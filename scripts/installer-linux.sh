#!/usr/bin/env bash
# Synly Linux 安装脚本.
#
# 安装, 升级与卸载共用这一个脚本, 幂等:
#   install.sh --silent --prefix "$HOME/.local"
#   install.sh --silent --wait-pid <pid> --log <日志> --result-file <结果文件>
#   install.sh --uninstall
#
# 升级按整目录交换: 新版新增的文件随暂存目录一起就位, 新版移除的文件随旧目录一起消失.
# 程序文件放在 <prefix>/opt/synly, 用户数据 (配置, 日志, 更新缓存) 不在安装目录里,
# 安装, 升级与卸载都不会触碰它.

set -euo pipefail

APP_NAME="synly"
APP_DISPLAY_NAME="Synly"
APP_CATEGORIES="Network;RemoteAccess;"
WAIT_PID_TIMEOUT_SECS=60

PREFIX="${HOME}/.local"
LOG_FILE=""
RESULT_FILE=""
WAIT_PID=""
RESTART=1
UNINSTALL=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PAYLOAD_DIR="$SCRIPT_DIR/payload"

usage() {
    cat <<'USAGE'
用法: install.sh [选项]

  --silent             非交互安装 (供应用内更新调用)
  --prefix DIR         安装前缀, 默认 $HOME/.local
  --wait-pid PID       等待该进程退出后再开始替换
  --log FILE           把脚本输出追加写入该文件
  --result-file FILE   失败原因写入该文件
  --no-restart         安装完成后不重新拉起应用
  --uninstall          卸载, 保留用户数据目录
USAGE
}

log() {
    printf '[install] %s\n' "$*"
}

write_result() {
    [[ -n "$RESULT_FILE" ]] || return 0
    mkdir -p "$(dirname "$RESULT_FILE")" 2>/dev/null || true
    printf '%s\n' "$1" >"$RESULT_FILE"
}

fail() {
    log "失败: $1"
    write_result "$1"
    exit 1
}

# 只删除脚本自己管理的路径, 避免参数异常时误删用户目录.
remove_tree() {
    local target="$1"
    case "$target" in
        "" | "/" | "$HOME" | "$PREFIX")
            log "拒绝删除危险路径: $target"
            return 1
            ;;
    esac
    rm -rf -- "$target"
}

install_dir() {
    printf '%s/opt/%s' "$PREFIX" "$APP_NAME"
}

staging_dir() {
    printf '%s/opt/.%s.staging-%s' "$PREFIX" "$APP_NAME" "$1"
}

backup_dir() {
    printf '%s/opt/.%s.old' "$PREFIX" "$APP_NAME"
}

wait_for_pid() {
    [[ -n "$WAIT_PID" ]] || return 0
    local waited=0
    while kill -0 "$WAIT_PID" 2>/dev/null; do
        if ((waited >= WAIT_PID_TIMEOUT_SECS)); then
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 0
}

install_icons() {
    local target="$1"
    local size source
    for size in 256 512; do
        source="$target/icons/$APP_NAME-$size.png"
        [[ -f "$source" ]] || continue
        local destination="$PREFIX/share/icons/hicolor/${size}x${size}/apps"
        mkdir -p "$destination"
        cp "$source" "$destination/$APP_NAME.png"
    done
}

write_desktop_entry() {
    local target="$1"
    local directory="$PREFIX/share/applications"
    mkdir -p "$directory"
    cat >"$directory/$APP_NAME.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$APP_DISPLAY_NAME
Comment=远程桌面串流
Exec=$target/$APP_NAME
Icon=$APP_NAME
Terminal=false
Categories=$APP_CATEGORIES
StartupWMClass=$APP_NAME
DESKTOP
}

refresh_launchers() {
    local target="$1"
    mkdir -p "$PREFIX/bin"
    ln -sfn "$target/$APP_NAME" "$PREFIX/bin/$APP_NAME"
    install_icons "$target"
    write_desktop_entry "$target"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 ||
            log "update-desktop-database 调用失败, 忽略"
    fi
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) log "提示: $PREFIX/bin 不在 PATH 中, 加入后可直接运行 $APP_NAME" ;;
    esac
}

relaunch() {
    ((RESTART)) || return 0
    local exe="$(install_dir)/$APP_NAME"
    [[ -x "$exe" ]] || return 0
    if command -v setsid >/dev/null 2>&1; then
        setsid "$exe" </dev/null >/dev/null 2>&1 &
    else
        nohup "$exe" </dev/null >/dev/null 2>&1 &
    fi
    log "已重新拉起 $exe"
}

install_app() {
    [[ -d "$PAYLOAD_DIR" ]] || fail "安装包缺少 payload 目录: $PAYLOAD_DIR"
    [[ -x "$PAYLOAD_DIR/$APP_NAME" ]] || fail "安装包里的主程序不可执行: $PAYLOAD_DIR/$APP_NAME"

    local version
    version="$(sed -n 's/^version=//p' "$PAYLOAD_DIR/install-manifest.txt" 2>/dev/null | head -n 1)"
    version="${version:-unknown}"

    local target="$(install_dir)"
    local staging="$(staging_dir "$version")"
    local backup="$(backup_dir)"

    log "安装 $APP_DISPLAY_NAME $version 到 $target"
    mkdir -p "$PREFIX/opt"
    remove_tree "$staging" || fail "无法清理暂存目录 $staging"
    cp -R "$PAYLOAD_DIR" "$staging" || fail "复制新版本到暂存目录失败"
    chmod 755 "$staging/$APP_NAME"

    local moved_old=0
    if [[ -d "$target" ]]; then
        remove_tree "$backup" || true
        if ! mv "$target" "$backup"; then
            remove_tree "$staging" || true
            fail "无法让出旧版本目录 $target"
        fi
        moved_old=1
    fi

    if ! mv "$staging" "$target"; then
        if ((moved_old)); then
            mv "$backup" "$target" || log "回滚失败, 旧版本仍在 $backup"
        fi
        remove_tree "$staging" || true
        fail "新版本就位失败, 已回滚到旧版本"
    fi

    refresh_launchers "$target"
    if ((moved_old)); then
        remove_tree "$backup" || log "旧版本备份未能删除, 可手动清理 $backup"
    fi
    if [[ -n "$RESULT_FILE" && -f "$RESULT_FILE" ]]; then
        rm -f "$RESULT_FILE"
    fi
    log "安装完成: $target"
}

uninstall_app() {
    if command -v pgrep >/dev/null 2>&1 && pgrep -x "$APP_NAME" >/dev/null 2>&1; then
        fail "$APP_DISPLAY_NAME 仍在运行, 请先退出后再卸载"
    fi
    local target="$(install_dir)"
    log "卸载 $target"
    if [[ -d "$target" ]]; then
        remove_tree "$target" || fail "无法删除安装目录 $target"
    fi
    rm -f "$PREFIX/bin/$APP_NAME" "$PREFIX/share/applications/$APP_NAME.desktop"
    local size
    for size in 256 512; do
        rm -f "$PREFIX/share/icons/hicolor/${size}x${size}/apps/$APP_NAME.png"
    done
    log "卸载完成, 用户数据目录保持不变"
}

main() {
    if [[ -n "$LOG_FILE" ]]; then
        mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true
        exec >>"$LOG_FILE" 2>&1
    fi
    if ((UNINSTALL)); then
        uninstall_app
        return 0
    fi
    if ! wait_for_pid; then
        fail "旧进程未在 ${WAIT_PID_TIMEOUT_SECS} 秒内退出, 请稍后重试更新"
    fi
    install_app
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --silent)
            shift
            ;;
        --prefix)
            PREFIX="${2:?--prefix 需要一个目录}"
            shift 2
            ;;
        --wait-pid)
            WAIT_PID="${2:?--wait-pid 需要一个进程号}"
            shift 2
            ;;
        --log)
            LOG_FILE="${2:?--log 需要一个文件路径}"
            shift 2
            ;;
        --result-file)
            RESULT_FILE="${2:?--result-file 需要一个文件路径}"
            shift 2
            ;;
        --no-restart)
            RESTART=0
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            printf '未知参数: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

main "$@"
