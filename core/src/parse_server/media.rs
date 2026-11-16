//! Audio/video handling for the parse server, mirroring mark-files-down
//! `api/workers.py`:
//!
//! * ffmpeg extracts audio as 16 kHz mono PCM16 WAV
//!   (`-ac 1 -ar 16000 -c:a pcm_s16le`);
//! * long recordings are split at silence midpoints found by
//!   `silencedetect`, otherwise at even cuts;
//! * FunASR (`POST {base}/asr`, form `file` + `spk`/`timestamp`), the
//!   OpenAI-compatible engine (`POST {base}/v1/audio/transcriptions`) and
//!   VibeVoice (`POST {base}/submit/url` + poll `GET /status/{id}`) are all
//!   configurable; VibeVoice is URL-based and needs `public_base_url`;
//! * video adds an OpenAI-compatible vision call
//!   (`POST {base}/chat/completions` with a `video_url` part) that turns the
//!   ASR transcript + frames into a timeline summary.
//!
//! External backends are **not** exercised by default: unit tests run
//! against a local mock HTTP server, and ffmpeg tests skip when ffmpeg is
//! absent.
use anyhow::{bail, Context, Result};
use sag_protocol::parse_settings::{AsrConfig, AsrEngine, VisionConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub speaker: String,
}

fn ffmpeg_cmd() -> String {
    std::env::var("FFMPEG_CMD").unwrap_or_else(|_| "ffmpeg".into())
}
fn ffprobe_cmd() -> String {
    std::env::var("FFPROBE_CMD").unwrap_or_else(|_| "ffprobe".into())
}

fn run_blocking<F, T>(f: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(f)
}

/// Audio duration in seconds via ffprobe; 0.0 when probing fails.
pub fn probe_duration(path: &Path) -> f64 {
    let output = std::process::Command::new(ffprobe_cmd())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Decode + resample to 16 kHz mono PCM16 WAV (mirrors `_to_wav16k`).
pub fn to_wav16k(src: &Path) -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let wav = dir.join(format!("sag-asr-{}.wav", uuid::Uuid::new_v4().simple()));
    let output = std::process::Command::new(ffmpeg_cmd())
        .args(["-y", "-v", "error", "-i"])
        .arg(src)
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(&wav)
        .output()
        .context("无法启动 ffmpeg")?;
    if !output.status.success()
        || !wav.exists()
        || std::fs::metadata(&wav).map(|m| m.len()).unwrap_or(0) == 0
    {
        let _ = std::fs::remove_file(&wav);
        bail!(
            "ffmpeg 转码 WAV 失败：{}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    Ok(wav)
}

/// Extract audio from a video file as 16 kHz mono WAV (mirrors
/// `_extract_audio`).
pub fn extract_audio(video: &Path, out: &Path) -> Result<PathBuf> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = std::process::Command::new(ffmpeg_cmd())
        .arg("-i")
        .arg(video)
        .args([
            "-vn",
            "-acodec",
            "pcm_s16le",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-y",
        ])
        .arg(out)
        .output()
        .context("无法启动 ffmpeg")?;
    if !output.status.success() {
        bail!(
            "ffmpeg 抽音失败（exit {}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    if !out.exists() {
        bail!("ffmpeg 成功返回但未生成音频文件");
    }
    Ok(out.to_path_buf())
}

fn silence_midpoints(wav: &Path, cfg: &AsrConfig) -> Vec<f64> {
    let output = std::process::Command::new(ffmpeg_cmd())
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(wav)
        .args([
            "-af",
            &format!(
                "silencedetect=noise={}dB:d={}",
                cfg.silence_noise_db, cfg.silence_min_seconds
            ),
            "-f",
            "null",
            "-",
        ])
        .output();
    let Ok(output) = output else { return vec![] };
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut midpoints = Vec::new();
    let mut pending: Option<f64> = None;
    for captures in regex::Regex::new(r"silence_(start|end):\s*(-?\d+(?:\.\d+)?)")
        .unwrap()
        .captures_iter(&text)
    {
        let kind = &captures[1];
        let value: f64 = captures[2].parse().unwrap_or(0.0);
        if kind == "start" {
            pending = Some(value);
        } else if let Some(start) = pending.take() {
            midpoints.push((start + value) / 2.0);
        }
    }
    midpoints
}

fn max_chunk_seconds(wav: &Path, duration: f64, cfg: &AsrConfig) -> f64 {
    // 16 kHz mono PCM16 = 32000 B/s; 10% headroom for multipart overhead.
    let bytes_per_sec = if duration > 0.0 {
        (std::fs::metadata(wav).map(|m| m.len()).unwrap_or(0) as f64 / duration).max(1.0)
    } else {
        32000.0
    };
    let by_bytes = ((cfg.max_upload_bytes as f64 / bytes_per_sec) * 0.9).max(30.0);
    cfg.chunk_seconds
        .min(cfg.max_clip_seconds)
        .min(by_bytes)
        .max(30.0)
}

fn split_wav(
    wav: &Path,
    duration: f64,
    max_seconds: f64,
    cfg: &AsrConfig,
) -> Result<Vec<(PathBuf, f64, f64)>> {
    if duration <= 0.0 || duration <= max_seconds {
        return Ok(vec![(wav.to_path_buf(), 0.0, duration)]);
    }
    let n = (duration / max_seconds).ceil() as usize;
    let target = duration / n as f64;
    let midpoints = silence_midpoints(wav, cfg);
    let mut cuts = Vec::new();
    let mut start = 0.0;
    while duration - start > max_seconds {
        let ideal = start + target;
        let lo = start + 1.0;
        let hi = (start + max_seconds).min(ideal + cfg.cut_search_window_secs);
        let cut = midpoints
            .iter()
            .copied()
            .filter(|m| *m >= lo && *m <= hi && *m > start)
            .min_by(|a, b| {
                (a - ideal)
                    .abs()
                    .partial_cmp(&(b - ideal).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or_else(|| ideal.min(start + max_seconds));
        cuts.push((cut * 1000.0).round() / 1000.0);
        start = cut;
    }
    let mut bounds = vec![0.0];
    bounds.extend(cuts);
    bounds.push(duration);
    let mut chunks = Vec::new();
    for window in bounds.windows(2) {
        let (s, e) = (window[0], window[1]);
        let chunk =
            std::env::temp_dir().join(format!("sag-asr-seg-{}.wav", uuid::Uuid::new_v4().simple()));
        let output = std::process::Command::new(ffmpeg_cmd())
            .args(["-y", "-v", "error", "-ss", &format!("{s:.3}"), "-i"])
            .arg(wav)
            .args([
                "-t",
                &format!("{:.3}", e - s),
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&chunk)
            .output()
            .context("无法启动 ffmpeg 切分")?;
        if !output.status.success() || std::fs::metadata(&chunk).map(|m| m.len()).unwrap_or(0) == 0
        {
            let _ = std::fs::remove_file(&chunk);
            bail!(
                "ffmpeg 切分音频失败：{}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
        chunks.push((chunk, s, e));
    }
    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Markdown formatters (mirrors workers.py `_format_funasr_markdown` /
// `_format_asr_markdown`).
// ---------------------------------------------------------------------------

pub fn format_funasr_markdown(segments: &[Segment]) -> String {
    let mut lines = vec!["# Audio Transcription".to_string(), String::new()];
    if segments.is_empty() {
        lines.push("*No speech detected.*".into());
        return lines.join("\n");
    }
    // Merge adjacent segments from the same speaker; OpenAI segments carry
    // no speaker and must keep their own timestamps.
    let mut merged: Vec<Segment> = Vec::new();
    for seg in segments {
        let mut merged_with_previous = false;
        if !seg.speaker.is_empty() {
            if let Some(last) = merged.last_mut() {
                if last.speaker == seg.speaker {
                    last.end = seg.end;
                    last.text.push_str(&seg.text);
                    merged_with_previous = true;
                }
            }
        }
        if !merged_with_previous {
            merged.push(seg.clone());
        }
    }
    for seg in merged {
        if seg.speaker.is_empty() {
            lines.push(format!("## [{} - {}]", seg.start, seg.end));
        } else {
            lines.push(format!("## [{} - {}] {}", seg.start, seg.end, seg.speaker));
        }
        if !seg.text.is_empty() {
            lines.push(seg.text.clone());
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

pub fn format_asr_markdown(text: &str) -> String {
    let mut lines = vec!["# Audio Transcription".to_string(), String::new()];
    let text = text.trim();
    if text.is_empty() {
        lines.push("*No speech detected.*".into());
        return lines.join("\n");
    }
    // JSON array / line-delimited JSON objects with Start/End/Speaker/Content.
    let mut entries: Vec<serde_json::Value> = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        match value {
            serde_json::Value::Array(items) => {
                entries = items.into_iter().filter(|v| v.is_object()).collect();
            }
            serde_json::Value::Object(_) => entries.push(value),
            _ => {}
        }
    }
    if entries.is_empty() {
        for line in text.lines() {
            let line = line.trim().trim_matches(',');
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                if value.is_object() {
                    entries.push(value);
                }
            }
        }
    }
    if entries.is_empty() {
        lines.push(text.to_string());
        return lines.join("\n");
    }
    for entry in entries {
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| entry.get(*k))
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        let start = get(&["Start time", "start_time", "Start", "start"]);
        let end = get(&["End time", "end_time", "End", "end"]);
        let speaker = get(&["Speaker ID", "speaker_id", "Speaker"]);
        let content = get(&["Content", "content"]);
        let mut header = Vec::new();
        if !start.is_empty() || !end.is_empty() {
            header.push(format!("[{start} - {end}]"));
        }
        if !speaker.is_empty() {
            header.push(speaker);
        }
        if !header.is_empty() {
            lines.push(format!("## {}", header.join(" ")));
        }
        if !content.is_empty() {
            lines.push(content);
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// ASR backends.
// ---------------------------------------------------------------------------

fn http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .build()?)
}

/// Minimal percent-encoding for a single form value.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn submit_funasr_file(
    client: &reqwest::Client,
    base: &str,
    audio: &Path,
) -> Result<Vec<Segment>> {
    let bytes = std::fs::read(audio)?;
    let filename = audio
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio.wav".into());
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(filename)
                .mime_str("application/octet-stream")?,
        )
        .text("spk", "true")
        .text("timestamp", "true");
    let response = client
        .post(format!("{}/asr", base.trim_end_matches('/')))
        .multipart(form)
        .send()
        .await?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.context("FunASR 返回了无效 JSON")?;
    if !status.is_success() {
        bail!(
            "FunASR 请求失败（HTTP {status}）：{}",
            value.to_string().chars().take(300).collect::<String>()
        );
    }
    let segments = value["segments"].as_array().cloned().unwrap_or_default();
    Ok(segments
        .iter()
        .filter_map(|s| {
            Some(Segment {
                text: s["text"].as_str().unwrap_or("").to_string(),
                start: s["start"].as_f64().unwrap_or(0.0),
                end: s["end"].as_f64().unwrap_or(0.0),
                speaker: s["speaker"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect())
}

async fn submit_openai_file(
    client: &reqwest::Client,
    base: &str,
    audio: &Path,
    cfg: &AsrConfig,
) -> Result<Vec<Segment>> {
    let wav = to_wav16k(audio)?;
    let mut owned: Vec<PathBuf> = Vec::new();
    let result = async {
        let duration = probe_duration(&wav);
        let max_seconds = max_chunk_seconds(&wav, duration, cfg);
        let chunks = split_wav(&wav, duration, max_seconds, cfg)?;
        owned = chunks
            .iter()
            .map(|(p, _, _)| p.clone())
            .filter(|p| p != &wav)
            .collect();
        let mut segments = Vec::new();
        for (index, (chunk, start, end)) in chunks.iter().enumerate() {
            let bytes = std::fs::read(chunk)?;
            let filename = if chunks.len() > 1 {
                format!("part{:03}.wav", index + 1)
            } else {
                "audio.wav".into()
            };
            let form = reqwest::multipart::Form::new()
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes)
                        .file_name(filename)
                        .mime_str("audio/wav")?,
                )
                .text("model", cfg.model.clone());
            let response = client
                .post(format!(
                    "{}/v1/audio/transcriptions",
                    base.trim_end_matches('/')
                ))
                .multipart(form)
                .send()
                .await?;
            let status = response.status();
            let value: serde_json::Value = response.json().await.context("ASR 返回了无效 JSON")?;
            if !status.is_success() {
                bail!(
                    "ASR 请求失败（HTTP {status}）：{}",
                    value.to_string().chars().take(300).collect::<String>()
                );
            }
            let text = value["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }
            segments.push(Segment {
                text,
                start: (start * 10.0).round() / 10.0,
                end: (end * 10.0).round() / 10.0,
                speaker: String::new(),
            });
        }
        Ok::<Vec<Segment>, anyhow::Error>(segments)
    }
    .await;
    for path in owned {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(&wav);
    result
}

async fn submit_vibevoice(
    client: &reqwest::Client,
    base: &str,
    audio_url: &str,
    cfg: &AsrConfig,
) -> Result<String> {
    let response = client
        .post(format!("{}/submit/url", base.trim_end_matches('/')))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("url={}", url_encode(audio_url)))
        .send()
        .await?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.context("VibeVoice 返回了无效 JSON")?;
    if !status.is_success() {
        bail!("VibeVoice 提交失败（HTTP {status}）");
    }
    let task_id = value["task_id"]
        .as_str()
        .context("VibeVoice 未返回 task_id")?
        .to_string();
    let mut consecutive_failures = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(
            cfg.poll_interval_secs.max(1),
        ))
        .await;
        let poll = client
            .get(format!("{}/status/{task_id}", base.trim_end_matches('/')))
            .send()
            .await;
        let Ok(response) = poll else {
            consecutive_failures += 1;
            if consecutive_failures >= 20 {
                bail!("VibeVoice 状态查询连续失败 20 次");
            }
            continue;
        };
        if !response.status().is_success() {
            consecutive_failures += 1;
            if consecutive_failures >= 20 {
                bail!("VibeVoice 状态查询连续失败 20 次");
            }
            continue;
        }
        consecutive_failures = 0;
        let data: serde_json::Value = response.json().await.context("VibeVoice 返回了无效 JSON")?;
        match data["task_status"].as_str().unwrap_or("") {
            "已完成" => return Ok(data["text"].as_str().unwrap_or("").to_string()),
            "处理失败" => {
                bail!(
                    "VibeVoice 转写失败：{}",
                    data["error"].as_str().unwrap_or("unknown")
                )
            }
            _ => {}
        }
    }
}

/// Transcribe an audio file with the configured engine. `audio_url` is only
/// used by VibeVoice (URL-based submission).
///
/// Sync entry point: the ASR calls build their own current-thread runtime,
/// so this must run on a blocking thread (the server dispatches jobs via
/// `tokio::task::spawn_blocking`), never directly from an async task.
pub fn transcribe_audio(
    audio: &Path,
    cfg: &AsrConfig,
    audio_url: Option<&str>,
) -> Result<Vec<Segment>> {
    if !cfg.enabled() {
        bail!("ASR 服务未配置（请填写 ASR 服务地址）");
    }
    let base = cfg.base_url.trim().to_string();
    let client = http_client(cfg.timeout_secs)?;
    match cfg.engine {
        AsrEngine::FunAsr => run_blocking(submit_funasr_file(&client, &base, audio)),
        AsrEngine::OpenAi => run_blocking(submit_openai_file(&client, &base, audio, cfg)),
        AsrEngine::VibeVoice => {
            let url =
                audio_url.context("VibeVoice 需要 public_base_url 提供可公开访问的音频 URL")?;
            let text = run_blocking(submit_vibevoice(&client, &base, url, cfg))?;
            // VibeVoice returns plain/JSON text; keep it as one segment so the
            // fun-asr formatter renders timestamps only when present.
            Ok(vec![Segment {
                text,
                start: 0.0,
                end: probe_duration(audio),
                speaker: String::new(),
            }])
        }
    }
}

/// Call an OpenAI-compatible vision model with a `video_url` part, mirroring
/// workers.py `_call_vision_model`. Returns the model's Markdown.
///
/// Same threading rule as `transcribe_audio`: blocking thread only.
pub fn call_vision(
    video_url: &str,
    asr_markdown: &str,
    custom_prompt: Option<&str>,
    cfg: &VisionConfig,
    api_key: Option<&str>,
) -> Result<String> {
    if !cfg.enabled() {
        bail!("视觉模型未配置（请填写视觉模型地址）");
    }
    let base = cfg.base_url.trim().to_string();
    let system = custom_prompt
        .filter(|p| !p.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.system_prompt.clone());
    let model = cfg.model.clone();
    let key = api_key.unwrap_or("not-needed").to_string();
    run_blocking(async move {
        let client = http_client(cfg.timeout_secs)?;
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": format!("这是视频的音频转录文本，请结合视频画面一起分析：\n\n{asr_markdown}")},
                {"role": "user", "content": [
                    {"type": "video_url", "video_url": {"url": video_url}},
                    {"type": "text", "text": "这是视频文件，请结合音频转录文本分析视频画面内容。"}
                ]}
            ],
            "max_tokens": 4096,
        });
        let response = client
            .post(format!(
                "{}/v1/chat/completions",
                base.trim_end_matches('/')
            ))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let value: serde_json::Value = response.json().await.context("视觉模型返回了无效 JSON")?;
        if !status.is_success() {
            bail!(
                "视觉模型请求失败（HTTP {status}）：{}",
                value.to_string().chars().take(300).collect::<String>()
            );
        }
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .context("视觉模型未返回正文")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn funasr_formatter_merges_same_speaker_only() {
        let segments = vec![
            Segment {
                text: "你好".into(),
                start: 0.0,
                end: 1.0,
                speaker: "SPK0".into(),
            },
            Segment {
                text: "世界".into(),
                start: 1.0,
                end: 2.0,
                speaker: "SPK0".into(),
            },
            Segment {
                text: "hi".into(),
                start: 2.0,
                end: 3.0,
                speaker: String::new(),
            },
        ];
        let md = format_funasr_markdown(&segments);
        assert!(md.contains("## [0 - 2] SPK0"));
        assert!(md.contains("你好世界"));
        assert!(md.contains("## [2 - 3]"));
        assert!(format_funasr_markdown(&[]).contains("No speech detected"));
    }

    #[test]
    fn asr_formatter_handles_json_and_plain_text() {
        let json = r#"[{"Start time": "00:00", "End time": "00:01", "Content": "hello"}]"#;
        let md = format_asr_markdown(json);
        assert!(md.contains("## [00:00 - 00:01]"));
        assert!(md.contains("hello"));
        assert!(format_asr_markdown("plain text").contains("plain text"));
    }

    #[test]
    fn ffmpeg_wav16k_round_trip_when_available() {
        if !has_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tone.mp3");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:a",
                "libmp3lame",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());
        let wav = to_wav16k(&src).unwrap();
        assert!(probe_duration(&wav) > 0.5);
        let bytes = std::fs::read(&wav).unwrap();
        // RIFF header, 16 kHz mono PCM16: byte rate field = 32000.
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 32000);
        let _ = std::fs::remove_file(&wav);
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn asr_backends_answer_from_mock_server() {
        let rt = runtime();
        rt.block_on(async {
            let mock = crate::parse_server::mock_mineru::MockMediaBackend::start().await;
            let base = mock.base_url();
            let dir = tempfile::tempdir().unwrap();
            let audio = dir.path().join("a.wav");
            std::fs::write(&audio, b"fake-wav-bytes").unwrap();

            let mut funasr = AsrConfig::default();
            funasr.engine = AsrEngine::FunAsr;
            funasr.base_url = base.clone();
            // Sync entry points build their own runtime: run them on a
            // blocking thread, exactly as the server dispatches jobs.
            let audio_c = audio.clone();
            let segments =
                tokio::task::spawn_blocking(move || transcribe_audio(&audio_c, &funasr, None))
                    .await
                    .expect("blocking task")
                    .unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].text, "hello funasr");
            assert_eq!(segments[0].speaker, "SPK0");

            let mut vibe = AsrConfig::default();
            vibe.engine = AsrEngine::VibeVoice;
            vibe.base_url = base.clone();
            vibe.poll_interval_secs = 1;
            let audio_c = audio.clone();
            let segments = tokio::task::spawn_blocking(move || {
                transcribe_audio(&audio_c, &vibe, Some("http://127.0.0.1:3001/uploads/a.wav"))
            })
            .await
            .expect("blocking task")
            .unwrap();
            assert_eq!(segments[0].text, "hello vibevoice");

            let mut vision = VisionConfig::default();
            vision.base_url = base.clone();
            let md = tokio::task::spawn_blocking(move || {
                call_vision(
                    "http://127.0.0.1:3001/uploads/v.mp4",
                    "# Audio Transcription\n\nhello",
                    None,
                    &vision,
                    None,
                )
            })
            .await
            .expect("blocking task")
            .unwrap();
            assert!(md.contains("Vision summary"), "{md}");
            mock.shutdown().await;
        });
    }

    #[test]
    fn openai_asr_answers_from_mock_server_when_ffmpeg_available() {
        if !has_ffmpeg() {
            return;
        }
        let rt = runtime();
        rt.block_on(async {
            let mock = crate::parse_server::mock_mineru::MockMediaBackend::start().await;
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("tone.mp3");
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:a",
                    "libmp3lame",
                ])
                .arg(&src)
                .status()
                .unwrap();
            assert!(status.success());
            let mut cfg = AsrConfig::default();
            cfg.engine = AsrEngine::OpenAi;
            cfg.base_url = mock.base_url();
            let src_c = src.clone();
            let segments =
                tokio::task::spawn_blocking(move || transcribe_audio(&src_c, &cfg, None))
                    .await
                    .expect("blocking task")
                    .unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].text, "hello openai");
            mock.shutdown().await;
        });
    }
}
