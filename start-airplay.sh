#!/bin/sh
# 小爱音箱端启动脚本：以后台方式常驻 AirPlay 接收端。
WORK_DIR=/data/open-xiaoai
BIN="$WORK_DIR/xiaoai-airplay"
CONF="$WORK_DIR/airplay.yaml"
LOG="$WORK_DIR/airplay.log"

PIDFILE="$WORK_DIR/airplay.pid"

case "$1" in
  start)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "airplay already running: $(cat "$PIDFILE")"; exit 0
    fi
    [ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }
    nohup "$BIN" "$CONF" >>"$LOG" 2>&1 &
    echo $! > "$PIDFILE"
    echo "airplay started: $(cat "$PIDFILE")"
    ;;
  stop)
    if [ -f "$PIDFILE" ]; then
      kill "$(cat "$PIDFILE")" 2>/dev/null
      rm -f "$PIDFILE"
    fi
    killall xiaoai-airplay 2>/dev/null || true
    echo "airplay stopped"
    ;;
  status)
    if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "running: $(cat "$PIDFILE")"
    else
      echo "not running"
    fi
    ;;
  *)
    echo "usage: $0 {start|stop|status}"
    ;;
esac
