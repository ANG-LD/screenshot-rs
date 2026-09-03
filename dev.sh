#!/usr/bin/env bash
# dev.sh —— 伪热重启开发辅助（源码在 FUSE/NTFS 上 cargo 会挂，故固定 CARGO_TARGET_DIR 到 /tmp）
#
# 用法:
#   ./dev.sh              # watch(默认):立即【构建 + 启动】应用,之后每次源码变更自动重编+重启
#   ./dev.sh build        # 只【构建 + 拷贝】一次(不启动、不监听)——改完代码后跑一次,再手动启动工具
#   ./dev.sh run          # 只启动(不重编)
#   ./dev.sh kill         # 只杀掉已启动的进程
#
# 环境变量:
#   RELEASE=1             # 用 release 编译(慢,但 scroll 时序准;验证 scroll 结果时用)
#   WATCH_DIRS="src"      # 监听目录(空格分隔),默认 src;可加第三_party/gpui-component 等
#
# 说明:
#   - 默认 debug 增量编译:只重编你改动的 crate,几秒~几十秒;release 每次都要重做全量 LTO,5 分钟。
#   - 自动重启会先 kill 掉正在跑的实例再拉起新窗口(避免 ETXTBSY:运行中的二进制不能覆盖)。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/screenshot-rs-target}"
RUN_BIN="$ROOT/target/release/screenshot-rs"
PID_FILE="$ROOT/.dev.pid"
LOG_FILE="$ROOT/.dev.log"
WATCH_DIRS="${WATCH_DIRS:-src}"

# 编译目标:默认 debug;RELEASE=1 或 DEV_PROFILE=--release 用 release
PROFILE="${DEV_PROFILE:-}"
[ "${RELEASE:-}" = "1" ] && PROFILE="--release"
if [ -n "$PROFILE" ]; then
  BUILT="$TARGET_DIR/release/screenshot-rs"
else
  BUILT="$TARGET_DIR/debug/screenshot-rs"
fi

# 源码目录下所有文件的最新 mtime(秒,含小数),用于轮询检测变更
newest() {
  local d
  local max=0
  for d in $WATCH_DIRS; do
    [ -e "$ROOT/$d" ] || continue
    local v
    v=$(find "$ROOT/$d" -type f -printf '%T@\n' 2>/dev/null | sort -rn | head -1)
    if [ -n "${v:-}" ]; then
      if awk -v a="$v" -v b="$max" 'BEGIN{exit !(a>b)}'; then max="$v"; fi
    fi
  done
  echo "$max"
}

# 杀死正在运行的实例(PID 文件 + 按进程名 screenshot-rs,含 cargo run 启动的),等它退出
kill_running() {
  if [ -f "$PID_FILE" ]; then
    local pid; pid=$(cat "$PID_FILE" 2>/dev/null || true)
    [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null || true
  fi
  # 按进程名精确匹配(避免把 dev.sh/bash 自己杀掉)
  pkill -x screenshot-rs 2>/dev/null || true
  # 等全部退出(最多 ~9s),再兜底强杀
  for _ in $(seq 1 30); do
    pgrep -x screenshot-rs >/dev/null 2>&1 || break
    sleep 0.3
  done
  pkill -9 -x screenshot-rs 2>/dev/null || true
  rm -f "$PID_FILE"
}

build() {
  local t0; t0=$(date +%s)
  echo "[dev] 编译 (${PROFILE:-debug}) ..."
  CARGO_TARGET_DIR="$TARGET_DIR" cargo build $PROFILE 2>&1 | tail -25
  local ec=${PIPESTATUS[0]}
  if [ "$ec" != "0" ]; then
    echo "[dev] 编译失败 (ec=$ec)"
    return 1
  fi
  [ -x "$BUILT" ] || { echo "[dev] 找不到产物: $BUILT"; return 1; }
  kill_running
  rm -f "$RUN_BIN"
  cp "$BUILT" "$RUN_BIN"
  echo "[dev] 完成 ($(( $(date +%s) - t0 ))s) → $RUN_BIN"
  echo "[dev] 产物路径: $RUN_BIN"
}

run() {
  kill_running
  sleep 0.4
  echo "[dev] 启动 $RUN_BIN (日志: $LOG_FILE)"
  local tries=0
  while [ "$tries" -lt 5 ]; do
    DISPLAY="${DISPLAY:-:1}" nohup "$RUN_BIN" >>"$LOG_FILE" 2>&1 &
    local pid=$!
    echo "$pid" > "$PID_FILE"
    sleep 0.4
    if kill -0 "$pid" 2>/dev/null; then
      echo "[dev] 已启动 pid=$pid"
      return 0
    fi
    # 启动失败(ETXTBSY 或立即崩溃),清理后重试
    rm -f "$PID_FILE"
    tries=$((tries+1))
    sleep 0.6
  done
  echo "[dev] 启动失败:请查看 $LOG_FILE"
  tail -5 "$LOG_FILE" 2>/dev/null || true
  return 1
}

# watch:立即构建+启动,然后每次变更自动重编+重启
watch() {
  echo "[dev] 构建并启动应用,开始监听 $WATCH_DIRS 变更 (Ctrl+C 退出)..."
  build && run
  local last; last=$(newest)
  while true; do
    sleep 1
    local cur; cur=$(newest)
    if [ -n "$last" ] && [ "$cur" != "$last" ]; then
      echo "[dev] 检测到源码变更,自动重编+重启..."
      last="$cur"
      build && run
    fi
  done
}

# 仅直接执行脚本时运行分发;source 本脚本(供函数复用)时不自动进入 watch
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-watch}" in
    build) build ;;
    run) run ;;
    kill) kill_running ;;
    watch) watch ;;
    *) echo "未知参数: $1 (build|run|kill|watch)"; exit 2 ;;
  esac
fi
