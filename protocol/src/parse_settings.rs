//! Parse-server-only settings. Lives in the protocol crate so the desktop
//! Settings JSON round-trips it, but the desktop UI does not have to know it.
use serde::{Deserialize, Serialize};

/// Default vision system prompt, copied from mark-files-down
/// `workers.py::DEFAULT_VISION_SYSTEM_PROMPT` (timeline summary shape).
pub const DEFAULT_VISION_SYSTEM_PROMPT: &str = "你是一个视频内容分析专家。根据提供的视频画面和音频转录文本，生成一份 Markdown 文档。\n\n## 要求\n\n1. 整合视频画面与音频转录文本的时间线，按时间顺序输出，用 `[MM:SS]` 标注时间点\n2. 将画面和语音自然融合，像亲历者一样叙述：同时看到了什么、听到了什么\n3. 开头用 2-3 句话概述视频主题\n\n## 示例\n\n# 视频内容总结\n\n简要概述视频主题和内容。\n\n## 详细时间线\n\n### [00:00] 开场\n讲解者站在白板前说道：\"大家好，今天我们来讨论人工智能的发展历程。\"背景是明亮的办公室，白板上画着时间轴。\n\n### [05:20] 主题讲解\n讲解者切换到 PPT，展示神经网络架构图，指着图中的节点说：\"这就是深度学习的核心概念——多层感知机。\"";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrEngine {
    #[default]
    FunAsr,
    OpenAi,
    VibeVoice,
}

/// Per-file-type preferred document engine (parse server only).
/// `Auto` keeps the historic chain order (MinerU → anydoc → Docling);
/// any other value moves that engine to the front of the chain for the
/// file type, the remaining engines still run as fallbacks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocEngine {
    #[default]
    Auto,
    Mineru,
    Anydoc,
    Docling,
}
impl DocEngine {
    /// Chain name used by `conversion::convert_with_engine`, or `None`
    /// for `Auto` (no reorder).
    pub fn chain_name(self) -> Option<&'static str> {
        match self {
            DocEngine::Auto => None,
            DocEngine::Mineru => Some("MinerU"),
            DocEngine::Anydoc => Some("anydoc"),
            DocEngine::Docling => Some("Docling"),
        }
    }
}

/// Which engine audio jobs try first. `Docling` is the historic
/// behaviour (Docling media first, ffmpeg + ASR on failure); `Asr`
/// skips Docling media and goes straight to the configured ASR backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioEngine {
    #[default]
    Docling,
    Asr,
}

/// Which engine video jobs try first. `Docling` is the historic
/// behaviour (Docling media first, ASR + vision timeline on failure);
/// `Timeline` skips Docling media and goes straight to ASR + vision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoEngine {
    #[default]
    Docling,
    Timeline,
}

/// Minimal `http(s)://host...` check (protocol crate has no URL parser).
fn valid_http_base(value: &str) -> bool {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && !rest.contains([' ', '?', '#'])
            && rest.split('/').next().is_some_and(|host| !host.is_empty())
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrConfig {
    pub engine: AsrEngine,
    /// Base URL of the ASR backend, e.g. `http://host:12346`. Empty = disabled.
    pub base_url: String,
    /// Model id for the OpenAI-compatible engine.
    pub model: String,
    pub timeout_secs: u64,
    /// Target chunk length for long recordings (seconds).
    pub chunk_seconds: f64,
    /// Per-request server upload cap (bytes). Mirrors mark-files-down's
    /// 25 MiB halogen limit.
    pub max_upload_bytes: u64,
    pub max_clip_seconds: f64,
    pub silence_noise_db: f64,
    pub silence_min_seconds: f64,
    pub cut_search_window_secs: f64,
    pub poll_interval_secs: u64,
}
impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            engine: AsrEngine::FunAsr,
            base_url: String::new(),
            model: "whisper-v3:turbo".into(),
            timeout_secs: 600,
            chunk_seconds: 600.0,
            max_upload_bytes: 25 * 1024 * 1024,
            max_clip_seconds: 1200.0,
            silence_noise_db: -35.0,
            silence_min_seconds: 0.3,
            cut_search_window_secs: 90.0,
            poll_interval_secs: 5,
        }
    }
}
impl AsrConfig {
    pub fn enabled(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=86400).contains(&self.timeout_secs) {
            return Err("ASR 超时应为 1–86400 秒".into());
        }
        if !(30.0..=3600.0).contains(&self.chunk_seconds) {
            return Err("ASR 切片目标时长应为 30–3600 秒".into());
        }
        if self.max_upload_bytes < 1024 * 1024 {
            return Err("ASR 单次上传上限至少 1 MiB".into());
        }
        if !(1..=60).contains(&self.poll_interval_secs) {
            return Err("ASR 轮询间隔应为 1–60 秒".into());
        }
        if !self.base_url.trim().is_empty() && !valid_http_base(self.base_url.trim()) {
            return Err("ASR 服务地址必须为 HTTP(S)".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    /// Base URL of the OpenAI-compatible vision backend, e.g.
    /// `http://host:12345/v1`. Empty = disabled.
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    /// Whether a vision API key is stored (credentials kind `vision`).
    /// Never serialized from the stored key itself.
    pub has_key: bool,
    pub system_prompt: String,
}
impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: "default-vision-model".into(),
            timeout_secs: 300,
            has_key: false,
            system_prompt: DEFAULT_VISION_SYSTEM_PROMPT.into(),
        }
    }
}
impl VisionConfig {
    pub fn enabled(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=86400).contains(&self.timeout_secs) {
            return Err("视觉模型超时应为 1–86400 秒".into());
        }
        if self.model.trim().is_empty() {
            return Err("视觉模型名称不能为空".into());
        }
        if !self.base_url.trim().is_empty() && !valid_http_base(self.base_url.trim()) {
            return Err("视觉模型地址必须为 HTTP(S)".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ParseServerSettings {
    /// Concurrent parse jobs. Default 4, range 1–16.
    pub queue_concurrency: usize,
    /// Explicit MinerU on/off switch for the parse server, persisted here
    /// — *not* in `Settings.mineru.enabled`, which `Store::settings()`
    /// recomputes from the desktop single-key slot on every load and
    /// would discard a PUT value.
    /// `None` = auto (the historic behaviour: MinerU runs when the key
    /// pool has a key); `Some(true/false)` = the user's explicit choice,
    /// read back verbatim by `GET /api/settings`.
    pub mineru_enabled: Option<bool>,
    /// How long a MinerU key that hit 429/quota stays out of rotation.
    pub mineru_key_cooldown_secs: u64,
    pub log_retention_days: u32,
    /// Output root for parsed Markdown. Empty = `<data>/parse-outputs`
    /// (with `docs/` and `temp/` underneath, mark-files-down layout).
    pub output_dir: String,
    /// Base URL under which this server's `/uploads` is reachable by
    /// external backends (VibeVoice ASR, vision models). Empty = unset;
    /// VibeVoice refuses to run without it.
    pub public_base_url: String,
    pub asr: AsrConfig,
    pub vision: VisionConfig,
    /// Preferred first engine per document type (`auto` = historic
    /// MinerU → anydoc → Docling order). The other engines still run
    /// as fallbacks; a preferred engine the file cannot use is skipped.
    pub pdf_engine: DocEngine,
    pub office_engine: DocEngine,
    pub image_engine: DocEngine,
    /// Which engine audio / video jobs try first.
    pub audio_engine: AudioEngine,
    pub video_engine: VideoEngine,
}
impl Default for ParseServerSettings {
    fn default() -> Self {
        Self {
            queue_concurrency: 4,
            mineru_enabled: None,
            mineru_key_cooldown_secs: 60,
            log_retention_days: 30,
            output_dir: String::new(),
            public_base_url: String::new(),
            asr: AsrConfig::default(),
            vision: VisionConfig::default(),
            pdf_engine: DocEngine::Auto,
            office_engine: DocEngine::Auto,
            image_engine: DocEngine::Auto,
            audio_engine: AudioEngine::Docling,
            video_engine: VideoEngine::Docling,
        }
    }
}
impl ParseServerSettings {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=16).contains(&self.queue_concurrency) {
            return Err("解析队列并发应为 1–16".into());
        }
        if !(0..=3600).contains(&self.mineru_key_cooldown_secs) {
            return Err("MinerU key 冷却时间应为 0–3600 秒".into());
        }
        if !(1..=365).contains(&self.log_retention_days) {
            return Err("日志保留天数应为 1–365".into());
        }
        self.asr.validate()?;
        self.vision.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_valid_and_round_trip() {
        let settings = ParseServerSettings::default();
        assert_eq!(settings.queue_concurrency, 4);
        assert!(settings.validate().is_ok());
        assert_eq!(settings.mineru_enabled, None);
        let restored: ParseServerSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(restored.queue_concurrency, 4);
        assert_eq!(restored.mineru_enabled, None);
        let explicit: ParseServerSettings =
            serde_json::from_str(r#"{"mineru_enabled":false}"#).unwrap();
        assert_eq!(explicit.mineru_enabled, Some(false));
        assert_eq!(restored.asr.model, "whisper-v3:turbo");
        let mut bad = settings.clone();
        bad.queue_concurrency = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn per_type_engine_preferences_round_trip() {
        let mut settings = ParseServerSettings::default();
        assert_eq!(settings.pdf_engine, DocEngine::Auto);
        assert_eq!(settings.pdf_engine.chain_name(), None);
        assert_eq!(DocEngine::Docling.chain_name(), Some("Docling"));
        settings.pdf_engine = DocEngine::Docling;
        settings.office_engine = DocEngine::Anydoc;
        settings.image_engine = DocEngine::Mineru;
        settings.audio_engine = AudioEngine::Asr;
        settings.video_engine = VideoEngine::Timeline;
        assert!(settings.validate().is_ok());
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["pdf_engine"], "docling");
        assert_eq!(json["office_engine"], "anydoc");
        assert_eq!(json["audio_engine"], "asr");
        assert_eq!(json["video_engine"], "timeline");
        let restored: ParseServerSettings = serde_json::from_value(json).unwrap();
        assert_eq!(restored.pdf_engine, DocEngine::Docling);
        assert_eq!(restored.audio_engine, AudioEngine::Asr);
        // Unknown values fail to parse instead of silently becoming Auto.
        assert!(serde_json::from_str::<ParseServerSettings>(
            r#"{"pdf_engine":"markitdown"}"#
        )
        .is_err());
    }
}
