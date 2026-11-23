//! Decoupled parse job: one input file becomes one Markdown output without
//! the desktop index pipeline.
//!
//! Document path reuses the existing engine chain unchanged
//! (`conversion::convert`: MinerU → anydoc → Docling, failure falls back to
//! the next engine). Audio/video first try Docling media through the same
//! chain, then fall back to ffmpeg + the configured ASR backend; video
//! additionally runs the vision model, mirroring mark-files-down
//! `workers.py` (`_video_worker`: ASR phase, then vision phase).
//!
//! Progress comes from the `conversion::Attempt` callback: current engine,
//! PDF segment (current/total) and elapsed time are persisted to the task
//! row on every callback.
use super::{
    keypool::{KeyPool, PoolEntry},
    media,
    store::ParseStore,
    types::{AttemptRecord, ParseJobRecord},
};
use anyhow::{bail, Context, Result};
use sag_protocol::types::Settings;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub struct JobOutcome {
    pub engine: String,
    pub pages: Option<u32>,
}

pub struct JobRunner {
    pub data_dir: PathBuf,
    pub uploads_dir: PathBuf,
    pub outputs_dir: PathBuf,
    pub store: Arc<ParseStore>,
    pub cancel: Arc<AtomicBool>,
}

struct MineruOutcome {
    attempted: bool,
    succeeded: bool,
    quota_error: Option<String>,
    pages: u32,
}

/// Effective MinerU switch for one job. An explicit parse-server choice
/// (`settings.parse.mineru_enabled`, set by `PUT /api/settings`) is
/// authoritative; with no explicit choice the historic auto-enable
/// applies — a key in the pool (or the desktop slot) turns MinerU on.
fn apply_mineru_enabled(settings: &mut Settings, pool_has_key: bool, desktop_has_key: bool) {
    match settings.parse.mineru_enabled {
        Some(explicit) => settings.mineru.enabled = explicit,
        None => {
            if pool_has_key || desktop_has_key {
                settings.mineru.enabled = true;
            }
        }
    }
}

impl JobRunner {
    pub fn input_path(&self, record: &ParseJobRecord) -> PathBuf {
        self.uploads_dir.join(&record.input_file)
    }
    pub fn output_path(&self, record: &ParseJobRecord) -> PathBuf {
        self.outputs_dir.join(&record.output_file)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Run one job to completion. Settings are a snapshot taken at job start;
    /// a settings change mid-job applies to the next job.
    pub fn run(&self, record: &ParseJobRecord, settings: &Settings) -> Result<JobOutcome> {
        if self.cancelled() {
            bail!("任务已取消");
        }
        let source = self.input_path(record);
        let output = self.output_path(record);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let outcome = if crate::parse_server::types::is_video_extension(&ext) {
            self.run_video(record, settings, &source, &output)
        } else if crate::parse_server::types::is_audio_extension(&ext) {
            self.run_audio(record, settings, &source, &output)
        } else {
            self.run_document(record, settings, &source, &output)
        };
        if self.cancelled() {
            bail!("任务已取消");
        }
        outcome
    }

    fn record_attempt(&self, task_id: &str, attempt: AttemptRecord) {
        if let Err(error) = self.store.record_attempt(task_id, &attempt) {
            eprintln!("解析进度持久化失败：{error:#}");
        }
    }

    fn run_document(
        &self,
        record: &ParseJobRecord,
        settings: &Settings,
        source: &Path,
        output: &Path,
    ) -> Result<JobOutcome> {
        // Plain-text fast path: txt/md/markdown/rst are already
        // Markdown-compatible and no conversion engine owns them (see
        // types::is_plain_text_extension). Read straight through so a
        // missing Docling component does not fail the most basic formats.
        let ext_for_text = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if crate::parse_server::types::is_plain_text_extension(&ext_for_text) {
            let bytes = std::fs::read(source)
                .with_context(|| format!("无法读取 {}", source.display()))?;
            let text = String::from_utf8_lossy(&bytes);
            std::fs::write(output, text.as_bytes())
                .with_context(|| format!("无法写入 {}", output.display()))?;
            self.record_attempt(
                &record.task_id,
                AttemptRecord {
                    engine: "text".to_string(),
                    phase: None,
                    segment_current: None,
                    segment_total: None,
                    elapsed_ms: 0,
                    error: None,
                },
            );
            return Ok(JobOutcome {
                engine: "text".to_string(),
                pages: None,
            });
        }
        let mut settings = settings.clone();
        // The parse server only ever talks to MinerU official
        // (mineru.net): the 302.AI provider is not offered here, so a
        // desktop-stored 302 provider must not leak into jobs, the key
        // pool, or the MinerU client. The stored value is left alone
        // (the desktop may still use 302); only this job's copy is
        // forced to official.
        settings.mineru.provider = sag_protocol::mineru::MineruProvider::Official;
        // Effective MinerU enablement: the pool (stored + env keys) counts
        // even when the desktop single-key slot is empty. An explicit
        // parse-server choice (`settings.parse.mineru_enabled`, set by
        // `PUT /api/settings`) wins over the historic auto-enable; with
        // no explicit choice the auto-enable behaviour is unchanged.
        let pool = KeyPool::new(
            self.data_dir.clone(),
            settings.mineru.provider,
            settings.parse.mineru_key_cooldown_secs,
            self.store.clone(),
        );
        let selected: Option<PoolEntry> = match pool.select() {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("MinerU key 池读取失败：{error:#}");
                None
            }
        };
        let desktop_has_key =
            crate::mineru::has_key(&self.data_dir, &settings.mineru).unwrap_or(false);
        apply_mineru_enabled(&mut settings, selected.is_some(), desktop_has_key);
        if let Some(entry) = &selected {
            crate::mineru::set_key_override(Some(entry.key.clone()));
        }
        let docling = crate::docling::Docling::new(&self.data_dir);
        let store = self.store.clone();
        let task_id = record.task_id.clone();
        let cancel = self.cancel.clone();
        let outcome_state = RefCell::new(MineruOutcome {
            attempted: false,
            succeeded: false,
            quota_error: None,
            pages: 0,
        });
        let last_success = RefCell::new(String::new());
        // Per-file-type preferred engine: PDF / Office / image each
        // choose their first engine; the rest of the chain still runs
        // as fallback. Text formats have no choice (anydoc only).
        let ext_for_engine = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let preferred = match crate::parse_server::types::doc_engine_group(&ext_for_engine) {
            Some("pdf") => settings.parse.pdf_engine.chain_name(),
            Some("office") => settings.parse.office_engine.chain_name(),
            Some("image") => settings.parse.image_engine.chain_name(),
            _ => None,
        };
        let result = crate::conversion::convert_with_engine(
            source,
            output,
            &docling,
            &settings,
            || cancel.load(Ordering::Relaxed),
            |attempt| {
                // Segmented PDFs report the overall snapshot in
                // `segment_status` (with the representative unfinished
                // segment); a single part also sets `segment` directly.
                let segment = attempt.segment.clone().or_else(|| {
                    attempt
                        .segment_status
                        .as_ref()
                        .and_then(|(_, segment)| segment.clone())
                });
                let seg_current = segment
                    .as_ref()
                    .map(|segment| u32::try_from(segment.current).unwrap_or(u32::MAX));
                let seg_total = segment
                    .as_ref()
                    .map(|segment| u32::try_from(segment.total).unwrap_or(u32::MAX));
                if let Some(total) = seg_total {
                    if total > 0 {
                        let mut state = outcome_state.borrow_mut();
                        if total > state.pages {
                            state.pages = total;
                        }
                    }
                }
                if attempt.engine == "MinerU" {
                    let mut state = outcome_state.borrow_mut();
                    state.attempted = true;
                    if attempt.phase.is_none() {
                        if attempt.error.is_none() {
                            state.succeeded = true;
                        } else if let Some(error) = &attempt.error {
                            if super::keypool::is_quota_error(error) && state.quota_error.is_none()
                            {
                                state.quota_error = Some(error.clone());
                            }
                        }
                    }
                }
                if attempt.phase.is_none() && attempt.error.is_none() && !attempt.engine.is_empty()
                {
                    *last_success.borrow_mut() = attempt.engine.clone();
                }
                let _ = store.record_attempt(
                    &task_id,
                    &AttemptRecord {
                        engine: attempt.engine.clone(),
                        phase: attempt.phase.clone(),
                        segment_current: seg_current,
                        segment_total: seg_total,
                        elapsed_ms: attempt.elapsed_ms,
                        error: attempt.error.clone(),
                    },
                );
            },
            preferred,
        );
        let state = outcome_state.borrow();
        if let Some(entry) = &selected {
            let pages = if state.pages > 0 {
                state.pages
            } else if state.succeeded {
                1
            } else {
                0
            };
            let error = state.quota_error.as_deref().or_else(|| {
                if !state.succeeded && state.attempted {
                    Some("MinerU 尝试失败")
                } else {
                    None
                }
            });
            if state.attempted {
                if let Err(error) = pool.report(entry.slot, state.succeeded, pages, error) {
                    eprintln!("MinerU key 统计写入失败：{error:#}");
                }
            }
        }
        drop(selected);
        crate::mineru::clear_key_override();
        let outcome = result.map(|_| JobOutcome {
            engine: last_success.borrow().clone(),
            pages: {
                let pages = state.pages;
                if pages > 0 {
                    Some(pages)
                } else {
                    None
                }
            },
        });
        outcome.map_err(|error| {
            if self.cancelled() {
                anyhow::anyhow!("任务已取消")
            } else {
                error
            }
        })
    }

    /// Docling media first; on failure, ffmpeg + configured ASR.
    /// `audio_engine = asr` skips Docling media and goes straight to ASR.
    fn run_audio(
        &self,
        record: &ParseJobRecord,
        settings: &Settings,
        source: &Path,
        output: &Path,
    ) -> Result<JobOutcome> {
        if !matches!(
            settings.parse.audio_engine,
            sag_protocol::parse_settings::AudioEngine::Asr
        ) {
            match self.run_document(record, settings, source, output) {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    self.record_attempt(
                        &record.task_id,
                        AttemptRecord {
                            engine: "Docling".into(),
                            phase: Some("media-fallback".into()),
                            segment_current: None,
                            segment_total: None,
                            elapsed_ms: 0,
                            error: Some(format!("{error:#}")),
                        },
                    );
                }
            }
            if self.cancelled() {
                bail!("任务已取消");
            }
        }
        let asr = &settings.parse.asr;
        let audio_url = self.public_url(&record.input_file, settings);
        let segments =
            media::transcribe_audio(source, asr, audio_url.as_deref()).context("ASR 转写失败")?;
        let markdown = if asr.engine == sag_protocol::parse_settings::AsrEngine::VibeVoice {
            media::format_asr_markdown(segments.first().map(|s| s.text.as_str()).unwrap_or(""))
        } else {
            media::format_funasr_markdown(&segments)
        };
        std::fs::write(output, markdown)?;
        Ok(JobOutcome {
            engine: "asr".into(),
            pages: None,
        })
    }

    /// Docling media first; on failure, ASR + vision timeline.
    /// `video_engine = timeline` skips Docling media and goes straight
    /// to ASR + vision.
    fn run_video(
        &self,
        record: &ParseJobRecord,
        settings: &Settings,
        source: &Path,
        output: &Path,
    ) -> Result<JobOutcome> {
        if !matches!(
            settings.parse.video_engine,
            sag_protocol::parse_settings::VideoEngine::Timeline
        ) {
            match self.run_document(record, settings, source, output) {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    self.record_attempt(
                        &record.task_id,
                        AttemptRecord {
                            engine: "Docling".into(),
                            phase: Some("media-fallback".into()),
                            segment_current: None,
                            segment_total: None,
                            elapsed_ms: 0,
                            error: Some(format!("{error:#}")),
                        },
                    );
                }
            }
            if self.cancelled() {
                bail!("任务已取消");
            }
        }
        let parse = &settings.parse;
        if !parse.asr.enabled() {
            bail!("ASR 服务未配置，视频任务无法转写音频");
        }
        let audio_path = self
            .outputs_dir
            .join("temp")
            .join(format!("{}-audio.wav", record.task_id));
        media::extract_audio(source, &audio_path).context("ffmpeg 抽音失败")?;
        let audio_url = self.public_url(&record.input_file, settings);
        let segments = media::transcribe_audio(&audio_path, &parse.asr, audio_url.as_deref())
            .context("ASR 转写失败")?;
        let _ = std::fs::remove_file(&audio_path);
        let asr_markdown = if parse.asr.engine == sag_protocol::parse_settings::AsrEngine::VibeVoice
        {
            media::format_asr_markdown(segments.first().map(|s| s.text.as_str()).unwrap_or(""))
        } else {
            media::format_funasr_markdown(&segments)
        };
        self.record_attempt(
            &record.task_id,
            AttemptRecord {
                engine: "ASR".into(),
                phase: Some("done".into()),
                segment_current: None,
                segment_total: None,
                elapsed_ms: 0,
                error: None,
            },
        );
        if !parse.vision.enabled() {
            bail!("视觉模型未配置，视频任务无法生成时间线总结（ASR 转写已完成）");
        }
        let video_url = self
            .public_url(&record.input_file, settings)
            .context("视觉模型需要 public_base_url 提供可公开访问的视频 URL")?;
        let vision_key = match crate::models::has_secret(&self.data_dir, "vision") {
            Ok(true) => crate::models::secret(&self.data_dir, "vision").ok(),
            _ => None,
        };
        let markdown = media::call_vision(
            &video_url,
            &asr_markdown,
            record.prompt.as_deref(),
            &parse.vision,
            vision_key.as_deref(),
        )
        .context("视觉模型调用失败")?;
        std::fs::write(output, markdown)?;
        Ok(JobOutcome {
            engine: "video".into(),
            pages: None,
        })
    }

    /// `public_base_url + /uploads/<input_file>`, or `None` when unset.
    fn public_url(&self, input_file: &str, settings: &Settings) -> Option<String> {
        let base = settings.parse.public_base_url.trim();
        if base.is_empty() {
            return None;
        }
        Some(format!(
            "{}/uploads/{input_file}",
            base.trim_end_matches('/')
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_server::{mock_mineru, store::ParseStore, types::JobStatus};

    fn record(task_id: &str, input_file: &str, output_file: &str, engine: &str) -> ParseJobRecord {
        ParseJobRecord {
            task_id: task_id.into(),
            input_md5: "mock".into(),
            input_file: input_file.into(),
            original_name: None,
            output_file: output_file.into(),
            client_id: None,
            engine: engine.into(),
            status: JobStatus::Pending,
            prompt: None,
            attempts: vec![],
            error: None,
            created_at: crate::parse_server::types::now_unix(),
            updated_at: crate::parse_server::types::now_unix(),
            started_at: None,
            finished_at: None,
            pages: None,
            progress: None,
        }
    }

    fn tiny_png() -> Vec<u8> {
        // 1x1 transparent PNG.
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn mineru_switch_explicit_choice_beats_key_pool_auto_enable() {
        // No explicit choice: pool/desktop key auto-enables (historic).
        let mut settings = Settings::default();
        assert!(!settings.mineru.enabled);
        apply_mineru_enabled(&mut settings, true, false);
        assert!(settings.mineru.enabled);
        let mut settings = Settings::default();
        apply_mineru_enabled(&mut settings, false, false);
        assert!(!settings.mineru.enabled);
        // Explicit off wins even when the pool has a key.
        let mut settings = Settings::default();
        settings.parse.mineru_enabled = Some(false);
        apply_mineru_enabled(&mut settings, true, true);
        assert!(!settings.mineru.enabled);
        // Explicit on wins even with no key at all.
        let mut settings = Settings::default();
        settings.parse.mineru_enabled = Some(true);
        apply_mineru_enabled(&mut settings, false, false);
        assert!(settings.mineru.enabled);
    }

    /// End-to-end: PNG via the mock MinerU online flow (batch submit →
    /// poll → zip with full.md). No real MinerU, no real key.
    #[tokio::test]
    async fn document_job_uses_mock_mineru_and_records_attempts() {
        let mock = mock_mineru::MockMineru::start("# Mock Title\n\nhello mock\n").await;
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let uploads = dir.path().join("uploads");
        let outputs = dir.path().join("outputs");
        std::fs::create_dir_all(&uploads).unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        let input_rel = "ab/cd/mockmd5.png";
        let input_abs = uploads.join(input_rel);
        std::fs::create_dir_all(input_abs.parent().unwrap()).unwrap();
        std::fs::write(&input_abs, tiny_png()).unwrap();

        let store = Arc::new(ParseStore::open(&data_dir.join("parse.db")).unwrap());
        // Pool key (fake, test-only) so the job installs it via the override.
        crate::parse_server::keypool::save_pool_key(
            &data_dir,
            sag_protocol::mineru::MineruProvider::Official,
            "mock-test-key-0000",
        )
        .unwrap();
        let runner = JobRunner {
            data_dir: data_dir.clone(),
            uploads_dir: uploads,
            outputs_dir: outputs.clone(),
            store: store.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let mut settings = Settings::default();
        settings.mineru.enabled = true;
        settings.mineru.provider = sag_protocol::mineru::MineruProvider::Official;
        settings.mineru.base_url = mock.base_url();
        let rec = record("mocktask", input_rel, "temp/mocktask.md", "mineru");
        // `conversion::convert` (MinerU) builds its own runtime: run the
        // job on a blocking thread, exactly as the server dispatches jobs.
        let outcome = tokio::task::spawn_blocking(move || runner.run(&rec, &settings))
            .await
            .expect("blocking task")
            .unwrap();
        assert_eq!(outcome.engine, "MinerU");
        let text = std::fs::read_to_string(outputs.join("temp/mocktask.md")).unwrap();
        assert!(text.contains("hello mock"), "{text}");
        // Key stats recorded one success for slot 0.
        let stat = store.key_stat("official", 0).unwrap();
        assert_eq!(stat.success, 1);
        mock.shutdown().await;
    }
}
