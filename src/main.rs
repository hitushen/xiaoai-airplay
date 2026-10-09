//! xiaoai-airplay (AP2 + 语音自动压低)
//!
//! 在 AP2 版基础上增加「语音抢音自动压低」：
//!   新发送设备接管时，底层 shairplay 会关闭旧发送端的 RTSP 控制连接，
//!   同时停止旧音频任务，避免旧设备“仍连接但无声”。
//!   监听小爱语音事件日志，检测到唤醒/识别开始 -> 把 AirPlay 输出 PCM 乘一个
//!   系数压低音量；检测到对话结束（或超过兜底时间）-> 恢复。
//! 仅缩放本程序输出的 PCM，不影响原生语音/唤醒，也不改系统音量。
//!
//! 音频链路：iPhone/Mac --AirPlay--> shairplay --> f32 PCM --> aplay --> ALSA

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use shairplay::{AirPlayMode, AudioFormat, AudioHandler, AudioSession, PairingStore, RaopServer};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

const DEFAULT_CONFIG: &str = "/data/open-xiaoai/airplay.yaml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    name: String,
    port: u16,
    password: String,
    hwaddr: String,
    device: String,
    aplay_path: String,
    format: String,
    mode: String,
    pin: String,
    pairing_store: String,
    led: LedConfig,
    duck: DuckConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "XiaoAI AirPlay".to_string(),
            port: 5000,
            password: String::new(),
            hwaddr: String::new(),
            device: "Playback".to_string(),
            aplay_path: "/usr/bin/aplay".to_string(),
            format: "S16_LE".to_string(),
            mode: "ap2".to_string(),
            pin: String::new(),
            pairing_store: "/data/open-xiaoai/airplay-pairing.json".to_string(),
            led: LedConfig::default(),
            duck: DuckConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DuckConfig {
    enabled: bool,
    /// 语音期间 AirPlay 音量系数。
    gain: f32,
    poll_ms: u64,
    /// 收到结束事件后延迟恢复。
    release_delay_ms: u64,
    /// 兜底最长压低时间。
    max_hold_ms: u64,
    log_files: Vec<String>,
    start_patterns: Vec<String>,
    end_patterns: Vec<String>,
}

impl Default for DuckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gain: 0.2,
            poll_ms: 200,
            release_delay_ms: 1500,
            max_hold_ms: 60000,
            log_files: vec![
                "/tmp/mico_aivs_lab/event.log".to_string(),
                "/tmp/mico_aivs_lab/instruction.log".to_string(),
                "/tmp/open-xiaoai/monitor.log".to_string(),
            ],
            start_patterns: vec![
                "\"namespace\":\"SpeechWakeup\"".to_string(),
                "\"name\":\"DuplexRecognizeStarted\"".to_string(),
                "唤醒词".to_string(),
            ],
            end_patterns: vec![
                "\"name\":\"Finish\",\"namespace\":\"Dialog\"".to_string(),
                "\"name\":\"ExitContinuousDialog\"".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct LedConfig {
    /// 播放时点亮音乐灯条。
    enabled: bool,
    /// 音乐灯 LED 编号（原生 mediaplayer 用 14）。
    music_id: u8,
    /// 点亮/熄灭命令模板，{id} 会替换为 music_id。
    show_command: String,
    shut_command: String,
}

impl Default for LedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            music_id: 14,
            show_command: "/bin/show_led".to_string(),
            shut_command: "/bin/shut_led".to_string(),
        }
    }
}

impl LedConfig {
    fn apply(&self, on: bool) {
        if !self.enabled {
            return;
        }
        let cmd = if on {
            &self.show_command
        } else {
            &self.shut_command
        };
        let script = format!("{cmd} {id}", id = self.music_id);
        let _ = std::process::Command::new("/bin/sh")
            .env("LED_PARENT", "xiaoai-airplay")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

impl Config {
    fn load(path: &str) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_yaml::from_str(&text).with_context(|| format!("解析配置失败：{path}"))
            }
            Err(_) => {
                info!(path, "配置文件不存在，使用默认配置");
                Ok(Self::default())
            }
        }
    }
    fn validate(&self) -> anyhow::Result<()> {
        if self.format != "S16_LE" {
            bail!("暂不支持 format: {}（只支持 S16_LE）", self.format);
        }
        if self.name.trim().is_empty() {
            bail!("name 不能为空");
        }
        Ok(())
    }
}

// ---------- 配对存储（文件持久化） ----------

struct FilePairingStore {
    path: PathBuf,
    inner: Mutex<PairingData>,
}

#[derive(Default, Serialize, Deserialize)]
struct PairingData {
    identity: Option<String>,
    devices: HashMap<String, String>,
}

impl FilePairingStore {
    fn new(path: &str) -> Self {
        let path = PathBuf::from(path);
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<PairingData>(&t).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }
    fn persist(&self, data: &PairingData) {
        if let Ok(bytes) = serde_json::to_vec_pretty(data) {
            let tmp = self.path.with_extension("json.tmp");
            if let Err(e) =
                std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &self.path))
            {
                warn!("配对存储写入失败：{e}");
            }
        }
    }
}

impl PairingStore for FilePairingStore {
    fn get(&self, device_id: &str) -> Option<[u8; 32]> {
        let d = self.inner.lock().ok()?;
        d.devices.get(device_id).and_then(|h| hex32(h))
    }
    fn put(&self, device_id: &str, public_key: [u8; 32]) {
        if let Ok(mut d) = self.inner.lock() {
            d.devices.insert(device_id.to_string(), to_hex(&public_key));
            let snap = PairingData {
                identity: d.identity.clone(),
                devices: d.devices.clone(),
            };
            drop(d);
            self.persist(&snap);
        }
    }
    fn remove(&self, device_id: &str) {
        if let Ok(mut d) = self.inner.lock() {
            d.devices.remove(device_id);
            let snap = PairingData {
                identity: d.identity.clone(),
                devices: d.devices.clone(),
            };
            drop(d);
            self.persist(&snap);
        }
    }
    fn has_any_pairing(&self) -> bool {
        self.inner
            .lock()
            .map(|d| !d.devices.is_empty())
            .unwrap_or(false)
    }
    fn load_identity(&self) -> Option<[u8; 32]> {
        self.inner.lock().ok()?.identity.as_deref().and_then(hex32)
    }
    fn save_identity(&self, seed: [u8; 32]) {
        if let Ok(mut d) = self.inner.lock() {
            d.identity = Some(to_hex(&seed));
            let snap = PairingData {
                identity: d.identity.clone(),
                devices: d.devices.clone(),
            };
            drop(d);
            self.persist(&snap);
        }
    }
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ---------- 播放状态 ----------

struct PlaybackState {
    volume_gain_bits: AtomicU32,
    duck_gain_bits: AtomicU32,
    ducked: AtomicBool,
    active_sessions: AtomicUsize,
    /// 当前唯一允许输出的 AirPlay 会话序号；新设备接管时使旧会话失效。
    active_session_id: AtomicU64,
    next_session_id: AtomicU64,
}

impl PlaybackState {
    fn new(duck_gain: f32) -> Self {
        Self {
            volume_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            duck_gain_bits: AtomicU32::new(duck_gain.clamp(0.0, 1.0).to_bits()),
            ducked: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            active_session_id: AtomicU64::new(0),
            next_session_id: AtomicU64::new(0),
        }
    }
    fn gain(&self) -> f32 {
        let v = f32::from_bits(self.volume_gain_bits.load(Ordering::Relaxed));
        if self.ducked.load(Ordering::Relaxed) {
            v * f32::from_bits(self.duck_gain_bits.load(Ordering::Relaxed))
        } else {
            v
        }
    }
    fn set_volume_db(&self, volume: f32) {
        self.volume_gain_bits
            .store(airplay_db_to_gain(volume).to_bits(), Ordering::Relaxed);
    }
    fn set_ducked(&self, d: bool) {
        if self.ducked.swap(d, Ordering::Relaxed) != d {
            info!(ducked = d, "AirPlay duck state changed");
        }
    }
    fn session_started(&self) -> (usize, u64) {
        let session_id = self.next_session_id.fetch_add(1, Ordering::AcqRel) + 1;
        let previous = self.active_sessions.fetch_add(1, Ordering::AcqRel);
        // AirPlay 接收端只保留一条输出链。后来的设备接管，旧会话在下一帧
        // 到来时主动关闭自己的 aplay，避免多个设备的 PCM 在 ALSA 中混音。
        self.active_session_id.store(session_id, Ordering::Release);
        (previous, session_id)
    }
    fn session_ended(&self) -> usize {
        self.active_sessions.fetch_sub(1, Ordering::AcqRel)
    }
    fn is_active(&self) -> bool {
        self.active_sessions.load(Ordering::Acquire) > 0
    }
    fn is_current_session(&self, session_id: u64) -> bool {
        self.active_session_id.load(Ordering::Acquire) == session_id
    }
}

// ---------- 语音事件监听 ----------

/// 跟随一个日志文件，只读取新增内容；启动时跳过历史，处理被截断/轮转的情况。
struct FileTail {
    path: PathBuf,
    offset: u64,
    primed: bool,
}

impl FileTail {
    fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
            offset: 0,
            primed: false,
        }
    }

    fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let len = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return out,
        };
        if !self.primed {
            // 先记录当前末尾，历史内容不触发压低
            self.offset = len;
            self.primed = true;
            return out;
        }
        if len < self.offset {
            self.offset = 0;
        } // 被截断或轮转
        if len == self.offset {
            return out;
        }
        if let Ok(mut f) = std::fs::File::open(&self.path) {
            if f.seek(SeekFrom::Start(self.offset)).is_ok() {
                let mut buf = String::new();
                if f.read_to_string(&mut buf).is_ok() {
                    self.offset += buf.len() as u64;
                    out.extend(buf.lines().map(|l| l.to_string()));
                }
            }
        }
        out
    }
}

async fn watch_voice(state: Arc<PlaybackState>, led: Arc<LedConfig>, cfg: DuckConfig) {
    if !cfg.enabled {
        info!("duck disabled");
        return;
    }
    info!(
        gain = cfg.gain,
        files = cfg.log_files.len(),
        "voice-duck watcher started"
    );
    let mut tails: Vec<FileTail> = cfg.log_files.iter().map(|p| FileTail::new(p)).collect();
    // 先 prime 一次，避免把旧日志当新事件
    for t in tails.iter_mut() {
        let _ = t.poll();
    }

    let poll = Duration::from_millis(cfg.poll_ms.max(50));
    let release = Duration::from_millis(cfg.release_delay_ms);
    let max_hold = Duration::from_millis(cfg.max_hold_ms);

    let mut start_at: Option<Instant> = None;
    let mut end_at: Option<Instant> = None;

    loop {
        sleep(poll).await;
        let mut saw_start = false;
        let mut saw_end = false;
        for t in tails.iter_mut() {
            for line in t.poll() {
                if cfg.end_patterns.iter().any(|p| line.contains(p.as_str())) {
                    saw_end = true;
                } else if cfg.start_patterns.iter().any(|p| line.contains(p.as_str())) {
                    saw_start = true;
                }
            }
        }
        let now = Instant::now();
        if saw_start {
            start_at = Some(now);
            end_at = None;
            if !state.ducked.load(Ordering::Relaxed) {
                state.set_ducked(true);
                info!("voice activity detected -> AirPlay ducked");
            }
        }
        if saw_end {
            end_at = Some(now);
        }
        if state.ducked.load(Ordering::Relaxed) {
            let released = end_at
                .map(|e| now.duration_since(e) >= release)
                .unwrap_or(false);
            let timed_out = start_at
                .map(|s| now.duration_since(s) >= max_hold)
                .unwrap_or(false);
            if released || timed_out {
                state.set_ducked(false);
                // 语音结束后原生灯不会自动切回音乐灯，这里重挂一次
                if led.enabled && state.is_active() {
                    led.apply(true);
                    info!("re-asserted music LED after voice interaction");
                }
                info!(timed_out, "voice activity ended -> AirPlay restored");
                start_at = None;
                end_at = None;
            }
        }
    }
}

// ---------- 主程序 ----------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());
    let config = Config::load(&path)?;
    config.validate()?;

    let state = Arc::new(PlaybackState::new(config.duck.gain));
    let handler = Arc::new(AplayHandler {
        config: Arc::new(config.clone()),
        state: state.clone(),
        led: Arc::new(config.led.clone()),
    });
    let store = Arc::new(FilePairingStore::new(&config.pairing_store));

    let mut builder = RaopServer::builder()
        .name(&config.name)
        .port(config.port)
        .pairing_store(store);
    if !config.password.trim().is_empty() {
        builder = builder.password(config.password.clone());
    }
    if !config.hwaddr.trim().is_empty() {
        builder = builder.hwaddr(parse_hwaddr(&config.hwaddr)?);
    }
    if !config.pin.trim().is_empty() {
        builder = builder.pin(config.pin.clone());
    }
    builder = builder.mode(match config.mode.to_ascii_lowercase().as_str() {
        "ap1" => AirPlayMode::AirPlay1,
        _ => AirPlayMode::AirPlay2,
    });

    let mut server = builder.build(handler).context("构建 AirPlay 服务失败")?;
    server.start().await.context("启动 AirPlay 服务失败")?;
    let port = server.service_info().port;
    info!(
        name = %config.name, port, mode = %config.mode,
        duck_gain = config.duck.gain, device = %config.device,
        "AirPlay receiver started"
    );

    // 语音事件监听任务
    tokio::spawn(watch_voice(
        state.clone(),
        Arc::new(config.led.clone()),
        config.duck.clone(),
    ));

    std::future::pending::<()>().await;
    Ok(())
}

struct AplayHandler {
    config: Arc<Config>,
    state: Arc<PlaybackState>,
    led: Arc<LedConfig>,
}

impl AudioHandler for AplayHandler {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        info!(codec = ?format.codec, channels = format.channels, bits = format.bits,
              sample_rate = format.sample_rate, "AirPlay audio session init");
        let (previous, session_id) = self.state.session_started();
        if previous == 0 {
            self.led.apply(true); // 播放开始：点亮音乐灯条
        } else {
            info!(
                session_id,
                previous_sessions = previous,
                "new AirPlay session took over previous session; old output and RTSP connection will be stopped"
            );
        }
        let (child, stdin) = spawn_aplay(&self.config, format);
        Box::new(AplaySession {
            child,
            stdin,
            led: self.led.clone(),
            frames_seen: AtomicU64::new(0),
            channels: u64::from(format.channels.max(1)),
            scratch: Vec::with_capacity(8192),
            state: self.state.clone(),
            session_id,
        })
    }
    fn on_volume(&self, volume: f32) {
        self.state.set_volume_db(volume);
        debug!(volume, "AirPlay volume changed");
    }
    fn on_client_connected(&self, addr: &str) {
        info!(addr, "AirPlay client connected");
    }
    fn on_client_disconnected(&self, addr: &str) {
        info!(addr, "AirPlay client disconnected");
    }
}

struct AplaySession {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames_seen: AtomicU64,
    channels: u64,
    scratch: Vec<u8>,
    state: Arc<PlaybackState>,
    led: Arc<LedConfig>,
    session_id: u64,
}

impl AudioSession for AplaySession {
    fn audio_process(&mut self, samples: &[f32]) {
        if !self.state.is_current_session(self.session_id) {
            // 新设备已经接管，关闭旧会话自己的 aplay，防止继续和新音频混音。
            self.stdin.take();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return;
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let gain = self.state.gain();
        self.scratch.clear();
        self.scratch.reserve(samples.len() * 2);
        for &sample in samples {
            let sample = (sample * gain).clamp(-1.0, 1.0);
            let pcm = (sample * i16::MAX as f32) as i16;
            self.scratch.extend_from_slice(&pcm.to_le_bytes());
        }
        if let Err(err) = stdin.write_all(&self.scratch) {
            warn!("写入 aplay 失败：{err}");
            self.stdin.take();
            return;
        }
        let frames = samples.len() as u64 / self.channels;
        let total = self.frames_seen.fetch_add(frames, Ordering::Relaxed) + frames;
        if total % 44_100 < frames.max(1) {
            debug!(frames = total, "AirPlay audio frames seen");
        }
    }
    fn audio_flush(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.flush();
        }
    }
}

impl Drop for AplaySession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let previous_sessions = self.state.session_ended();
        if previous_sessions == 1 {
            self.led.apply(false); // 最后一个 AirPlay 会话结束：熄灭音乐灯条
        }
        info!("AirPlay audio session ended");
    }
}

fn spawn_aplay(config: &Config, format: AudioFormat) -> (Option<Child>, Option<ChildStdin>) {
    let mut command = Command::new(&config.aplay_path);
    command
        .arg("-q")
        .arg("-D")
        .arg(&config.device)
        .arg("-t")
        .arg("raw")
        .arg("-f")
        .arg(&config.format)
        .arg("-c")
        .arg(format.channels.max(1).to_string())
        .arg("-r")
        .arg(format.sample_rate.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match command.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            info!(path = %config.aplay_path, device = %config.device,
                  channels = format.channels.max(1), sample_rate = format.sample_rate,
                  "started AirPlay aplay backend");
            (Some(child), stdin)
        }
        Err(err) => {
            error!("启动 aplay 失败：{err}");
            (None, None)
        }
    }
}

fn parse_hwaddr(raw: &str) -> anyhow::Result<Vec<u8>> {
    let hex = raw
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() != 12 {
        bail!("hwaddr 必须是 6 字节（12 个十六进制字符）");
    }
    let mut out = Vec::with_capacity(6);
    for idx in (0..12).step_by(2) {
        let byte = u8::from_str_radix(&hex[idx..idx + 2], 16)
            .with_context(|| format!("hwaddr 字节非法：{}", &hex[idx..idx + 2]))?;
        out.push(byte);
    }
    Ok(out)
}

fn airplay_db_to_gain(volume: f32) -> f32 {
    if volume <= -144.0 {
        0.0
    } else {
        10.0f32.powf(volume.min(0.0) / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_airplay_session_supersedes_previous_session() {
        let state = PlaybackState::new(0.2);
        let (previous, first) = state.session_started();
        assert_eq!(previous, 0);
        assert!(state.is_current_session(first));

        let (previous, second) = state.session_started();
        assert_eq!(previous, 1);
        assert!(!state.is_current_session(first));
        assert!(state.is_current_session(second));

        assert_eq!(state.session_ended(), 2);
        assert_eq!(state.session_ended(), 1);
    }
}

#[cfg(test)]
mod single_session_tests {
    use super::*;

    #[test]
    fn newest_session_is_the_only_output_owner() {
        let state = PlaybackState::new(0.2);
        let (first_previous, first) = state.session_started();
        assert_eq!(first_previous, 0);
        assert!(state.is_current_session(first));
        let (second_previous, second) = state.session_started();
        assert_eq!(second_previous, 1);
        assert!(!state.is_current_session(first));
        assert!(state.is_current_session(second));
        assert_eq!(state.session_ended(), 2);
        assert!(state.is_current_session(second));
        assert_eq!(state.session_ended(), 1);
    }
}
