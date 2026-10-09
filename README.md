# xiaoai-airplay

让已经取得 SSH 权限的小爱音箱（当前部署目标为 OH2P）作为 AirPlay 接收端，保留原生音频链路、音乐灯条和语音抢音自动压低。

## 功能

- AirPlay 1 / AirPlay 2 接收（当前默认配置为 AP2）。
- AirPlay PCM 经过 `aplay -D default` 输出，保留小爱原生 `vis/ledd` 灯效链路。
- 语音唤醒期间自动压低 AirPlay 音量，结束后恢复。
- 新发送设备接管时，停止旧音频任务并**真正关闭旧设备的 RTSP 控制连接**，避免旧设备显示仍连接但没有声音。
- 同一台 iPhone 在 AirPlay 协商期间可能使用多个并行 RTSP 连接；接管逻辑按发送端 IP 保留当前发送端的并行连接，只踢掉其他发送端。

## 目录

```text
src/main.rs                         AirPlay 应用、ALSA 输出、LED/ducking
vendor/shairplay/                   vendored shairplay 0.10，含连接踢出补丁
start-airplay.sh                    音箱端启动/停止/状态脚本
airplay.example.yaml                配置示例
```

## 连接接管实现

补丁位于 `vendor/shairplay/src/`：

1. 每个 RTSP TCP 连接创建一个 `watch` 取消接收端，并登记到发送端 IP 的连接表。
2. AirPlay 1 的有效 `SETUP`，以及 AirPlay 2 的 realtime/buffered 音频 `SETUP`，调用 `takeover_sender()`。
3. `takeover_sender()` 向其他发送端 IP 的连接发送关闭信号。
4. `net/server.rs::process_connection()` 同时等待 TCP 读取和关闭信号；收到信号后退出任务，TCP stream 被 Drop，旧设备的 AirPlay 控制连接随之关闭。
5. AP2 音频任务仍通过 `set_active_audio()` 停止，防止旧 RTP/音频任务继续写入 ALSA。

这不是只杀 `aplay` 的静音方案，而是同时收回音频任务和 RTSP 控制连接。

## 构建

需要 Rust 1.94 或更新版本（本地验证使用 Rust 1.99）。

```bash
cargo test --offline
cargo build --release
```

`shairplay` 使用本地 vendor 路径，避免依赖上游源码状态漂移。项目本身不包含账号、密码、Token 或设备私钥。

## 音箱端部署

先备份线上二进制，再替换。示例：

```sh
cd /data/open-xiaoai
cp -p xiaoai-airplay "xiaoai-airplay.bak.$(date +%Y%m%d-%H%M%S)"
./start-airplay.sh stop
# 传入并核对新二进制 SHA-256 后再执行：
chmod +x xiaoai-airplay.new
mv xiaoai-airplay.new xiaoai-airplay
./start-airplay.sh start
./start-airplay.sh status
```

验证重点：设备 A 播放时让设备 B 接管，A 应在 AirPlay 控制端断开，B 正常出声；然后再验证灯条呼吸、语音自动压低、语音结束恢复和原生小爱功能。

## 许可

应用代码使用 LGPL-3.0-or-later；vendor 中的 `shairplay` 保留其上游 LGPL-3.0 许可和版权说明。
