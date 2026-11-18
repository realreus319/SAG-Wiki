//! Parse-only HTTP server: `0.0.0.0:3001`, no auth (intranet service).
//!
//! Routes (shapes copied from mark-files-down `api/main.py` — note its
//! README mentions a `POST /file_parse` that does not exist in code, so it
//! is **not** implemented here either):
//!
//! * `GET /`, `GET /ui`, `GET /ui/*` — static Web UI
//!   (`core/src/parse_server/ui/`, embedded at compile time)
//! * `GET /health`
//! * `POST /tasks` — multipart form: `input_file` (path under uploads),
//!   optional `output_file`, `task_id`, `client_id`
//! * `POST /convert` — multipart `files[]`, or `task_id` + `prompt` re-run
//! * `GET /tasks/{id}`, `GET /tasks/{id}/result`, `POST /tasks/{id}/restart`
//!   (failed/completed only), `POST /tasks/{id}/cancel`
//! * `GET /uploads/...` — read-only, so external ASR/vision backends can
//!   fetch the file by URL
//! * `GET/PUT /api/settings`, `GET /api/settings/mineru-keys`,
//!   `POST/DELETE /api/settings/mineru-keys[/…]`, `GET /api/logs`,
//!   `GET /api/tasks` — paginated task list (status/client_id/search
//!   filters) for the Web UI task overview page
//!
//! Every submit endpoint accepts `client_id` both as a form field and as
//! the `X-Client-Id` header (form wins); it is stored on the task record
//! and echoed in structured log lines.
use super::{
    job::{JobOutcome, JobRunner},
    keypool::{self, KeyPool},
    store::ParseStore,
    types::{
        default_output_relative_path, engine_for_extension, md5_file, md5_hex, multipart_boundary,
        now_unix, parse_multipart, resolve_task_engine, safe_relative_path, upload_relative_path,
        JobStatus, ParseJobRecord,
    },
};
use anyhow::{bail, Context, Result};
use http_body_util::{combinators::BoxBody, BodyExt, Full, Limited};
use hyper::{
    body::{Bytes, Incoming},
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use sag_protocol::types::Settings;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;

type HttpResponse = Response<BoxBody<Bytes, std::convert::Infallible>>;

// Web UI files, embedded at compile time so the server works from any
// working directory. Frontend files themselves are untouched; all requests
// in the UI are same-origin relative paths.
const UI_INDEX_HTML: &str = include_str!("ui/index.html");
const UI_STYLE_CSS: &str = include_str!("ui/style.css");
const UI_APP_JS: &str = include_str!("ui/app.js");

fn static_response(content_type: &str, body: &str) -> HttpResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body.to_owned())).boxed())
        .unwrap()
}

fn json_response(status: StatusCode, value: Value) -> HttpResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(serde_json::to_vec(&value).unwrap())).boxed())
        .unwrap()
}

fn error_response(status: StatusCode, message: impl ToString) -> HttpResponse {
    json_response(status, json!({"error": message.to_string()}))
}

fn task_json(record: &ParseJobRecord) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or(json!({}));
    if let Value::Object(ref mut map) = value {
        map.insert("status".into(), json!(record.status.as_str()));
    }
    value
}

pub struct ParseServer {
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub uploads_dir: PathBuf,
    pub outputs_dir: PathBuf,
    store: Arc<crate::store::Store>,
    jobs: Arc<ParseStore>,
    queue_tx: tokio::sync::mpsc::Sender<String>,
    queue_rx: Mutex<Option<tokio::sync::mpsc::Receiver<String>>>,
    semaphore: RwLock<Arc<Semaphore>>,
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl ParseServer {
    pub fn open(data_dir: PathBuf, host: String, port: u16) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&data_dir)?;
        let uploads_dir = data_dir.join("parse-uploads");
        let outputs_dir = data_dir.join("parse-outputs");
        std::fs::create_dir_all(&uploads_dir)?;
        std::fs::create_dir_all(outputs_dir.join("docs"))?;
        std::fs::create_dir_all(outputs_dir.join("temp"))?;
        let store = Arc::new(crate::store::Store::open(&data_dir.join("sag.sqlite"))?);
        let jobs = Arc::new(ParseStore::open(&data_dir.join("parse.db"))?);
        let settings = store.settings()?;
        let (queue_tx, queue_rx) = tokio::sync::mpsc::channel::<String>(1024);
        let server = Arc::new(Self {
            data_dir,
            host,
            port,
            uploads_dir,
            outputs_dir,
            store,
            jobs,
            queue_tx,
            queue_rx: Mutex::new(Some(queue_rx)),
            semaphore: RwLock::new(Arc::new(Semaphore::new(
                settings.parse.queue_concurrency.max(1),
            ))),
            cancels: Mutex::new(HashMap::new()),
        });
        // Startup recovery: unfinished jobs go back to pending and requeue.
        let recovered = server.restore()?;
        server.log(
            "info",
            None,
            None,
            &format!("解析服务启动，恢复 {recovered} 个未完成任务"),
            None,
            None,
        )?;
        // Prebuilt Docling components (e.g. baked into a Docker image) are
        // staged into the data directory on first start. Only runs for
        // profiles that are not installed yet and whose
        // SAG_DOCLING_*_COMPONENT_DIR variable points at a directory;
        // nothing is downloaded. Runs in the background so startup stays
        // fast; a task that needs Docling before staging finishes still
        // reports the component as missing, exactly as before.
        server.maybe_stage_prebuilt_docling();
        Ok(server)
    }

    fn maybe_stage_prebuilt_docling(self: &Arc<Self>) {
        use sag_protocol::DoclingProfile;
        let docling = std::sync::Arc::new(crate::docling::Docling::new(&self.data_dir));
        let mut wanted: Vec<DoclingProfile> = Vec::new();
        for (profile, var) in [
            (DoclingProfile::Documents, "SAG_DOCLING_COMPONENT_DIR"),
            (DoclingProfile::Media, "SAG_DOCLING_MEDIA_COMPONENT_DIR"),
        ] {
            if docling.has_profile(profile) {
                continue;
            }
            match std::env::var_os(var) {
                Some(dir) if std::path::Path::new(&dir).is_dir() => wanted.push(profile),
                Some(dir) => {
                    let _ = self.log(
                        "warn",
                        None,
                        None,
                        &format!(
                            "预装 Docling 组件目录不存在：{}={}",
                            var,
                            dir.to_string_lossy()
                        ),
                        Some("docling"),
                        None,
                    );
                }
                None => {}
            }
        }
        if wanted.is_empty() {
            return;
        }
        let server = self.clone();
        std::thread::spawn(move || {
            let _ = server.log(
                "info",
                None,
                None,
                "正在从预装组件准备 Docling（本地复制，不下载）…",
                Some("docling"),
                None,
            );
            if let Err(error) = docling.start_profiles(wanted) {
                let _ = server.log(
                    "error",
                    None,
                    None,
                    &format!("预装 Docling 组件准备失败：{error:#}"),
                    Some("docling"),
                    None,
                );
                return;
            }
            loop {
                let status = docling.status();
                if !status.running {
                    if status.stage == "done" {
                        let _ = server.log(
                            "info",
                            None,
                            None,
                            "Docling 组件已就绪。",
                            Some("docling"),
                            None,
                        );
                    } else if let Some(error) = status.error {
                        let _ = server.log(
                            "error",
                            None,
                            None,
                            &format!("预装 Docling 组件准备失败：{error}"),
                            Some("docling"),
                            None,
                        );
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
    }

    pub fn settings(&self) -> Result<Settings> {
        self.store.settings()
    }

    fn log(
        &self,
        level: &str,
        task_id: Option<&str>,
        client_id: Option<&str>,
        message: &str,
        engine: Option<&str>,
        elapsed_ms: Option<u64>,
    ) -> Result<()> {
        self.jobs
            .log(level, task_id, client_id, message, engine, elapsed_ms)
    }

    fn restore(&self) -> Result<usize> {
        let unfinished = self.jobs.unfinished()?;
        let count = unfinished.len();
        for record in unfinished {
            self.jobs
                .update_status(&record.task_id, JobStatus::Pending, None)?;
            self.jobs.update_engine(
                &record.task_id,
                &resolve_task_engine(&record.input_file, &record.engine),
            )?;
            let _ = self.queue_tx.try_send(record.task_id);
        }
        Ok(count)
    }

    fn enqueue(&self, task_id: &str) {
        if let Err(error) = self.queue_tx.try_send(task_id.to_string()) {
            eprintln!("解析任务入队失败 {task_id}：{error}");
        }
    }

    fn refresh_semaphore(&self) -> Result<()> {
        if let Ok(settings) = self.settings() {
            let mut slot = self.semaphore.write().unwrap();
            *slot = Arc::new(Semaphore::new(settings.parse.queue_concurrency.max(1)));
        }
        Ok(())
    }

    // -- task submission ---------------------------------------------------

    /// Shared dedup logic (mirrors `file_handler.upload_and_create_task`):
    /// same content MD5 returns the old task id when it is pending,
    /// processing, or completed with its output still on disk; a failed or
    /// output-less record is dropped and re-created.
    fn dedup_or_create(
        &self,
        md5: &str,
        input_rel: &str,
        output_rel: Option<&str>,
        engine: &str,
        prompt: Option<String>,
        client_id: Option<String>,
        task_id_override: Option<String>,
    ) -> Result<(String, bool)> {
        if let Some(existing) = self.jobs.get_by_md5(md5)? {
            match existing.status {
                JobStatus::Completed => {
                    if self.outputs_dir.join(&existing.output_file).exists() {
                        return Ok((existing.task_id, false));
                    }
                    self.jobs.delete(&existing.task_id)?;
                }
                JobStatus::Pending | JobStatus::Processing => {
                    return Ok((existing.task_id, false));
                }
                JobStatus::Failed => {
                    self.jobs.delete(&existing.task_id)?;
                }
            }
        }
        let task_id = task_id_override.unwrap_or_else(|| md5.to_string());
        if self.jobs.get(&task_id)?.is_some() {
            bail!("task_id 已存在：{task_id}");
        }
        let output_file = match output_rel {
            Some(rel) => format!("docs/{rel}"),
            None => default_output_relative_path(&task_id)
                .to_string_lossy()
                .replace('\\', "/"),
        };
        let now = now_unix();
        let record = ParseJobRecord {
            task_id: task_id.clone(),
            input_md5: md5.into(),
            input_file: input_rel.into(),
            output_file,
            client_id,
            engine: engine.into(),
            status: JobStatus::Pending,
            prompt,
            attempts: vec![],
            error: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            pages: None,
            progress: None,
        };
        self.jobs.insert(&record)?;
        self.log(
            "info",
            Some(&task_id),
            record.client_id.as_deref(),
            &format!("任务已提交 engine={engine} input={input_rel}"),
            Some(engine),
            None,
        )?;
        self.enqueue(&task_id);
        Ok((task_id, true))
    }

    /// Store an uploaded file under the layered MD5 path and create the task.
    fn submit_bytes(
        &self,
        filename: &str,
        bytes: &[u8],
        prompt: Option<String>,
        client_id: Option<String>,
    ) -> Result<(String, bool)> {
        let ext = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let engine = engine_for_extension(&ext).with_context(|| format!("不支持的格式：.{ext}"))?;
        let md5 = md5_hex(bytes);
        let rel = upload_relative_path(&md5, &format!(".{ext}"));
        let dest = self.uploads_dir.join(&rel);
        if !dest.exists() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, bytes)?;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        self.dedup_or_create(&md5, &rel_str, None, engine, prompt, client_id, None)
    }

    // -- HTTP ---------------------------------------------------------------

    pub async fn serve(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        let listener = TcpListener::bind((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("无法监听 {}:{}", self.host, self.port))?;
        eprintln!("解析服务监听 http://{}:{}", self.host, self.port);
        let dispatcher = self.clone();
        let mut queue_rx = self.queue_rx.lock().unwrap().take().unwrap();
        let dispatch_shutdown = shutdown.clone();
        tokio::spawn(async move {
            dispatcher
                .dispatch_loop(&mut queue_rx, dispatch_shutdown)
                .await;
        });
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let server = self.clone();
                    let stop = shutdown.clone();
                    tokio::spawn(async move {
                        let handler = hyper::service::service_fn(move |req| {
                            let server = server.clone();
                            let stop = stop.clone();
                            async move { Ok::<_, std::convert::Infallible>(server.handle(req, stop).await) }
                        });
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder.timer(TokioTimer::new()).header_read_timeout(std::time::Duration::from_secs(10));
                        if let Err(error) = builder.serve_connection(TokioIo::new(stream), handler).await {
                            if !error.is_incomplete_message() && !error.is_closed() {
                                eprintln!("解析服务连接结束：{error}");
                            }
                        }
                    });
                }
            }
        }
        Ok(())
    }

    /// Dispatcher: one permit per running job from the current semaphore, so
    /// a queue-concurrency change applies to newly started jobs.
    async fn dispatch_loop(
        self: Arc<Self>,
        queue_rx: &mut tokio::sync::mpsc::Receiver<String>,
        shutdown: CancellationToken,
    ) {
        while let Some(task_id) = tokio::select! {
            _ = shutdown.cancelled() => None,
            item = queue_rx.recv() => item,
        } {
            let Ok(record) = self.jobs.get(&task_id) else {
                continue;
            };
            let Some(record) = record else { continue };
            if record.status != JobStatus::Pending {
                continue; // Cancelled or restarted while queued.
            }
            let permit = {
                let semaphore = self.semaphore.read().unwrap().clone();
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    permit = semaphore.acquire_owned() => match permit {
                        Ok(permit) => permit,
                        Err(_) => return,
                    },
                }
            };
            let server = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                server.run_job_task(task_id).await;
            });
        }
    }

    async fn run_job_task(&self, task_id: String) {
        let Ok(Some(record)) = self.jobs.get(&task_id) else {
            return;
        };
        if record.status != JobStatus::Pending {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .unwrap()
            .insert(task_id.clone(), cancel.clone());
        let started = std::time::Instant::now();
        if let Err(error) = self
            .jobs
            .update_status(&task_id, JobStatus::Processing, None)
        {
            eprintln!("任务状态更新失败：{error:#}");
            return;
        }
        let settings = match self.settings() {
            Ok(settings) => settings,
            Err(error) => {
                let _ = self.fail_job(&task_id, &format!("读取设置失败：{error:#}"), started);
                return;
            }
        };
        let runner = JobRunner {
            data_dir: self.data_dir.clone(),
            uploads_dir: self.uploads_dir.clone(),
            outputs_dir: self.outputs_dir.clone(),
            store: self.jobs.clone(),
            cancel: cancel.clone(),
        };
        let record_for_run = record.clone();
        let outcome =
            tokio::task::spawn_blocking(move || runner.run(&record_for_run, &settings)).await;
        self.cancels.lock().unwrap().remove(&task_id);
        match outcome {
            Ok(Ok(JobOutcome { engine, pages })) => {
                let elapsed = started.elapsed().as_millis() as u64;
                // The record's engine is the routing guess from submit time;
                // replace it with the engine that actually produced the output.
                let _ = self.jobs.update_engine(&task_id, &engine);
                let _ = self
                    .jobs
                    .update_status(&task_id, JobStatus::Completed, None);
                let _ = self.log(
                    "info",
                    Some(&task_id),
                    record.client_id.as_deref(),
                    &format!("任务完成 engine={engine} pages={pages:?}"),
                    Some(&engine),
                    Some(elapsed),
                );
            }
            Ok(Err(error)) => {
                let _ = self.fail_job(&task_id, &format!("{error:#}"), started);
            }
            Err(error) => {
                let _ = self.fail_job(&task_id, &format!("任务执行中断：{error}"), started);
            }
        }
    }

    fn fail_job(&self, task_id: &str, message: &str, started: std::time::Instant) -> Result<()> {
        let record = self.jobs.get(task_id)?.unwrap_or_else(|| ParseJobRecord {
            task_id: task_id.into(),
            input_md5: String::new(),
            input_file: String::new(),
            output_file: String::new(),
            client_id: None,
            engine: String::new(),
            status: JobStatus::Failed,
            prompt: None,
            attempts: vec![],
            error: None,
            created_at: now_unix(),
            updated_at: now_unix(),
            started_at: None,
            finished_at: None,
            pages: None,
            progress: None,
        });
        let elapsed = started.elapsed().as_millis() as u64;
        self.jobs
            .update_status(task_id, JobStatus::Failed, Some(message))?;
        // Full failure text is kept, never swallowed.
        self.log(
            "error",
            Some(task_id),
            record.client_id.as_deref(),
            message,
            None,
            Some(elapsed),
        )?;
        Ok(())
    }

    async fn handle(&self, req: Request<Incoming>, _stop: CancellationToken) -> HttpResponse {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        match (method.clone(), path.as_str()) {
            (Method::GET, "/health") => return self.health(),
            (Method::GET, "/") => return Self::handle_ui("/"),
            (Method::GET, p) if p == "/ui" || p.starts_with("/ui/") => return Self::handle_ui(p),
            (Method::POST, "/tasks") => return self.handle_submit(req).await,
            (Method::POST, "/convert") => return self.handle_convert(req).await,
            (Method::GET, p) if p.starts_with("/tasks/") => {
                return self.handle_task_get(&method, p, &query).await
            }
            (Method::POST, p) if p.starts_with("/tasks/") => return self.handle_task_post(p).await,
            (Method::GET, "/api/settings") => return self.handle_settings_get(),
            (Method::PUT, "/api/settings") => return self.handle_settings_put(req).await,
            (Method::GET, "/api/tasks") => return self.handle_tasks_list(&query),
            (Method::GET, "/api/settings/mineru-keys") => return self.handle_keys_get(),
            (Method::POST, "/api/settings/mineru-keys") => return self.handle_keys_post(req).await,
            (Method::DELETE, p) if p.starts_with("/api/settings/mineru-keys") => {
                return self.handle_keys_delete(p, &query)
            }
            (Method::GET, "/api/logs") => return self.handle_logs(&query),
            (Method::GET, p) if p.starts_with("/uploads/") => return self.handle_upload_get(p),
            _ => return error_response(StatusCode::NOT_FOUND, "接口不存在"),
        }
    }

    /// Serve the embedded Web UI. `/` and `/ui[/]` both return the SPA
    /// shell; assets use absolute `/ui/` paths so all entry URLs work.
    fn handle_ui(path: &str) -> HttpResponse {
        match path {
            "/" | "/ui" | "/ui/" | "/ui/index.html" => {
                static_response("text/html; charset=utf-8", UI_INDEX_HTML)
            }
            "/ui/style.css" => static_response("text/css; charset=utf-8", UI_STYLE_CSS),
            "/ui/app.js" => static_response("application/javascript; charset=utf-8", UI_APP_JS),
            _ => error_response(StatusCode::NOT_FOUND, "页面不存在"),
        }
    }

    fn health(&self) -> HttpResponse {
        let counts = self.jobs.counts().unwrap_or(json!({}));
        json_response(
            StatusCode::OK,
            json!({
                "status": "ok",
                "service": "sag-parse-server",
                "uploads_dir": self.uploads_dir.to_string_lossy(),
                "outputs_dir": self.outputs_dir.to_string_lossy(),
                "tasks": counts,
            }),
        )
    }

    fn client_id_from(req: &Request<Incoming>, fields: &HashMap<String, String>) -> Option<String> {
        fields
            .get("client_id")
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .or_else(|| {
                req.headers()
                    .get("X-Client-Id")
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_owned)
            })
    }

    async fn read_body(&self, req: Request<Incoming>, limit: usize) -> Result<Bytes, HttpResponse> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            Limited::new(req.into_body(), limit).collect(),
        )
        .await
        {
            Ok(Ok(collected)) => Ok(collected.to_bytes()),
            Ok(Err(error)) => Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("请求体过大或读取失败：{error}"),
            )),
            Err(_) => Err(error_response(
                StatusCode::REQUEST_TIMEOUT,
                "请求体接收超时",
            )),
        }
    }

    fn form_fields(parts: &[super::types::MultipartPart]) -> HashMap<String, String> {
        parts
            .iter()
            .filter(|p| p.filename.is_none())
            .map(|p| (p.name.clone(), p.text()))
            .collect()
    }

    async fn handle_submit(&self, req: Request<Incoming>) -> HttpResponse {
        let content_type = req
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let Some(boundary) = multipart_boundary(&content_type) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Content-Type 必须为 multipart/form-data",
            );
        };
        let client_id_header = req
            .headers()
            .get("X-Client-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let limit = self
            .settings()
            .map(|s| s.runtime.conversion_input_mib * 1024 * 1024 + 16 * 1024 * 1024)
            .unwrap_or(512 * 1024 * 1024);
        let body = match self.read_body(req, limit).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let parts = match parse_multipart(&body, &boundary) {
            Ok(parts) => parts,
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
        };
        let fields = Self::form_fields(&parts);
        let client_id = fields
            .get("client_id")
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .or(client_id_header);
        let input_file = fields.get("input_file").cloned().unwrap_or_default();
        if input_file.trim().is_empty() {
            return error_response(StatusCode::BAD_REQUEST, "input_file 不能为空");
        }
        let Some(input_rel) = safe_relative_path(&input_file) else {
            return error_response(StatusCode::BAD_REQUEST, "input_file 路径不合法");
        };
        let input_rel = input_rel.to_string_lossy().replace('\\', "/");
        let full = self.uploads_dir.join(&input_rel);
        if !full.exists() {
            return error_response(StatusCode::BAD_REQUEST, format!("文件不存在：{input_rel}"));
        }
        let md5 = match md5_file(&full) {
            Ok(md5) => md5,
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        let ext = Path::new(&input_rel)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let Some(engine) = engine_for_extension(&ext) else {
            return error_response(StatusCode::BAD_REQUEST, format!("不支持的格式：.{ext}"));
        };
        let output_file = fields
            .get("output_file")
            .filter(|v| !v.trim().is_empty())
            .cloned();
        if let Some(output) = &output_file {
            if safe_relative_path(output).is_none() {
                return error_response(StatusCode::BAD_REQUEST, "output_file 路径不合法");
            }
        }
        let task_id = fields
            .get("task_id")
            .filter(|v| !v.trim().is_empty())
            .cloned();
        let prompt = fields
            .get("prompt")
            .filter(|v| !v.trim().is_empty())
            .cloned();
        match self.dedup_or_create(
            &md5,
            &input_rel,
            output_file.as_deref(),
            engine,
            prompt,
            client_id,
            task_id,
        ) {
            Ok((task_id, _)) => json_response(StatusCode::ACCEPTED, json!({"task_id": task_id})),
            Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
        }
    }

    async fn handle_convert(&self, req: Request<Incoming>) -> HttpResponse {
        let content_type = req
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let Some(boundary) = multipart_boundary(&content_type) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Content-Type 必须为 multipart/form-data",
            );
        };
        let client_id_header = req
            .headers()
            .get("X-Client-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let limit = self
            .settings()
            .map(|s| s.runtime.conversion_input_mib * 1024 * 1024 + 16 * 1024 * 1024)
            .unwrap_or(512 * 1024 * 1024);
        let body = match self.read_body(req, limit).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let parts = match parse_multipart(&body, &boundary) {
            Ok(parts) => parts,
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
        };
        let fields = Self::form_fields(&parts);
        let client_id = fields
            .get("client_id")
            .filter(|v| !v.trim().is_empty())
            .cloned()
            .or(client_id_header);
        let prompt = fields
            .get("prompt")
            .filter(|v| !v.trim().is_empty())
            .cloned();
        let task_id = fields
            .get("task_id")
            .filter(|v| !v.trim().is_empty())
            .cloned();
        let files: Vec<_> = parts.iter().filter(|p| p.filename.is_some()).collect();

        // Mode 2: task_id + prompt re-run (mirrors mark-files-down).
        if let Some(task_id) = task_id.clone() {
            if !files.is_empty() {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"results": [{"status": "error", "message": "files 和 task_id 不能同时提供"}]}),
                );
            }
            let Some(prompt) = prompt.clone() else {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"results": [{"status": "error", "message": "task_id 模式必须提供 prompt"}]}),
                );
            };
            return match self.reprocess_with_prompt(&task_id, &prompt, client_id) {
                Ok(new_id) => json_response(
                    StatusCode::OK,
                    json!({"results": [{"task_id": new_id, "status": "success"}]}),
                ),
                Err(error) => json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"results": [{"status": "error", "message": error.to_string()}]}),
                ),
            };
        }
        if files.is_empty() {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"results": [{"status": "error", "message": "请提供 files 或 task_id"}]}),
            );
        }
        let mut results = Vec::new();
        for file in files {
            let filename = file.filename.clone().unwrap_or_default();
            if filename.is_empty() {
                continue;
            }
            match self.submit_bytes(&filename, &file.data, prompt.clone(), client_id.clone()) {
                Ok((task_id, _)) => results
                    .push(json!({"filename": filename, "task_id": task_id, "status": "success"})),
                Err(error) => results.push(
                    json!({"filename": filename, "status": "error", "message": error.to_string()}),
                ),
            }
        }
        json_response(StatusCode::OK, json!({"results": results}))
    }

    /// Deterministic reprocess id: `original + md5(prompt)[:16]`
    /// (mirrors mark-files-down `reprocess_with_prompt`).
    fn reprocess_with_prompt(
        &self,
        original_task_id: &str,
        prompt: &str,
        client_id: Option<String>,
    ) -> Result<String> {
        let original = self
            .jobs
            .get(original_task_id)?
            .with_context(|| format!("任务不存在：{original_task_id}"))?;
        let prompt_hash = md5_hex(prompt.as_bytes());
        let new_task_id = format!("{}{}", original_task_id, &prompt_hash[..16]);
        if let Some(existing) = self.jobs.get(&new_task_id)? {
            match existing.status {
                JobStatus::Completed => {
                    if self.outputs_dir.join(&existing.output_file).exists() {
                        return Ok(new_task_id);
                    }
                    self.jobs.delete(&new_task_id)?;
                }
                JobStatus::Pending | JobStatus::Processing => return Ok(new_task_id),
                JobStatus::Failed => {
                    self.jobs.delete(&new_task_id)?;
                }
            }
        }
        let input_abs = self.uploads_dir.join(&original.input_file);
        if !input_abs.exists() {
            bail!("源文件不存在：{}", original.input_file);
        }
        let md5 = md5_file(&input_abs)?;
        let engine = resolve_task_engine(&original.input_file, &original.engine);
        let (task_id, _) = self.dedup_or_create(
            &md5,
            &original.input_file,
            None,
            &engine,
            Some(prompt.to_string()),
            client_id,
            Some(new_task_id),
        )?;
        Ok(task_id)
    }

    async fn handle_task_get(&self, method: &Method, path: &str, _query: &str) -> HttpResponse {
        if *method != Method::GET {
            return error_response(StatusCode::METHOD_NOT_ALLOWED, "仅支持 GET");
        }
        let rest = path.trim_start_matches("/tasks/");
        if let Some(task_id) = rest.strip_suffix("/result") {
            let task_id = task_id.trim_end_matches('/');
            return self.task_result(task_id);
        }
        let task_id = rest.trim_end_matches('/');
        match self.jobs.get(task_id) {
            Ok(Some(record)) => json_response(StatusCode::OK, task_json(&record)),
            Ok(None) => error_response(StatusCode::NOT_FOUND, "任务不存在"),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }

    fn task_result(&self, task_id: &str) -> HttpResponse {
        let record = match self.jobs.get(task_id) {
            Ok(Some(record)) => record,
            Ok(None) => return error_response(StatusCode::NOT_FOUND, "任务不存在"),
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        match record.status {
            JobStatus::Pending => json_response(
                StatusCode::OK,
                json!({"task_id": task_id, "status": "pending"}),
            ),
            JobStatus::Processing => json_response(
                StatusCode::OK,
                json!({"task_id": task_id, "status": "processing"}),
            ),
            JobStatus::Failed => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"task_id": task_id, "status": "failed", "error": record.error}),
            ),
            JobStatus::Completed => {
                let full = self.outputs_dir.join(&record.output_file);
                match std::fs::read_to_string(&full) {
                    Ok(markdown) => json_response(
                        StatusCode::OK,
                        json!({"task_id": task_id, "status": "completed", "markdown": markdown}),
                    ),
                    Err(error) => error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("输出文件不存在：{error}"),
                    ),
                }
            }
        }
    }

    async fn handle_task_post(&self, path: &str) -> HttpResponse {
        let rest = path.trim_start_matches("/tasks/");
        if let Some(task_id) = rest.strip_suffix("/restart") {
            return self.task_restart(task_id.trim_end_matches('/'));
        }
        if let Some(task_id) = rest.strip_suffix("/cancel") {
            return self.task_cancel(task_id.trim_end_matches('/'));
        }
        error_response(StatusCode::NOT_FOUND, "接口不存在")
    }

    fn task_restart(&self, task_id: &str) -> HttpResponse {
        let record = match self.jobs.get(task_id) {
            Ok(Some(record)) => record,
            Ok(None) => return error_response(StatusCode::NOT_FOUND, "任务不存在"),
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        if !matches!(record.status, JobStatus::Failed | JobStatus::Completed) {
            return error_response(StatusCode::BAD_REQUEST, "任务正在执行中，无法重启");
        }
        if !self.uploads_dir.join(&record.input_file).exists() {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("源文件不存在：{}", record.input_file),
            );
        }
        let engine = resolve_task_engine(&record.input_file, &record.engine);
        if let Err(error) = self.jobs.reset_for_restart(task_id, &engine) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
        let _ = self.log(
            "info",
            Some(task_id),
            record.client_id.as_deref(),
            "任务已重启",
            Some(&engine),
            None,
        );
        self.enqueue(task_id);
        json_response(
            StatusCode::OK,
            json!({"task_id": task_id, "status": "pending"}),
        )
    }

    fn task_cancel(&self, task_id: &str) -> HttpResponse {
        let record = match self.jobs.get(task_id) {
            Ok(Some(record)) => record,
            Ok(None) => return error_response(StatusCode::NOT_FOUND, "任务不存在"),
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        match record.status {
            JobStatus::Pending => {
                // The dispatcher skips non-pending rows, so marking it
                // failed here is enough to keep it from ever running.
                if let Err(error) =
                    self.jobs
                        .update_status(task_id, JobStatus::Failed, Some("任务已取消"))
                {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
                json_response(
                    StatusCode::OK,
                    json!({"task_id": task_id, "status": "failed", "error": "任务已取消"}),
                )
            }
            JobStatus::Processing => {
                if let Some(token) = self.cancels.lock().unwrap().get(task_id) {
                    token.store(true, Ordering::Relaxed);
                }
                json_response(
                    StatusCode::OK,
                    json!({"task_id": task_id, "status": "processing", "cancel_requested": true}),
                )
            }
            _ => error_response(StatusCode::BAD_REQUEST, "任务已结束，无法取消"),
        }
    }

    /// `GET /api/tasks?status=&client_id=&search=&page=&page_size=` — the
    /// task list the Web UI overview page (`ui/app.js` `fetchTaskList`)
    /// polls. Parameter and field names follow the frontend's existing
    /// convention: `page` (default 1), `page_size` (default 20, max 100),
    /// `status` one of pending/processing/completed/failed, `client_id`
    /// exact match, `search` fuzzy on input filename / task_id.
    fn handle_tasks_list(&self, query: &str) -> HttpResponse {
        let params: HashMap<String, String> = query
            .split('&')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((percent_decode(key), percent_decode(value)))
            })
            .collect();
        let status = params
            .get("status")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty());
        if let Some(status) = status {
            if !matches!(status, "pending" | "processing" | "completed" | "failed") {
                return error_response(StatusCode::BAD_REQUEST, format!("status 无效：{status}"));
            }
        }
        let client_id = params
            .get("client_id")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty());
        let search = params
            .get("search")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty());
        let page = params
            .get("page")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let page_size = params
            .get("page_size")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(20)
            .clamp(1, 100);
        match self.jobs.list(status, client_id, search, page, page_size) {
            Ok((tasks, total)) => {
                let total_pages = ((total + page_size - 1) / page_size).max(1);
                json_response(
                    StatusCode::OK,
                    json!({
                        "tasks": tasks.iter().map(list_task_json).collect::<Vec<_>>(),
                        "total": total,
                        "page": page,
                        "page_size": page_size,
                        "total_pages": total_pages,
                    }),
                )
            }
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }

    fn handle_upload_get(&self, path: &str) -> HttpResponse {
        let rel = path.trim_start_matches("/uploads/");
        let Some(safe) = safe_relative_path(rel) else {
            return error_response(StatusCode::BAD_REQUEST, "路径不合法");
        };
        let full = self.uploads_dir.join(&safe);
        match std::fs::read(&full) {
            Ok(bytes) => {
                let mime = match full
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "mp4" => "video/mp4",
                    "mov" => "video/quicktime",
                    "webm" => "video/webm",
                    "mkv" => "video/x-matroska",
                    "mp3" => "audio/mpeg",
                    "wav" => "audio/wav",
                    "pdf" => "application/pdf",
                    _ => "application/octet-stream",
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", mime)
                    .body(Full::new(Bytes::from(bytes)).boxed())
                    .unwrap()
            }
            Err(_) => error_response(StatusCode::NOT_FOUND, "文件不存在"),
        }
    }

    // -- settings API ------------------------------------------------------

    fn outputs_root(&self, settings: &Settings) -> PathBuf {
        let dir = settings.parse.output_dir.trim();
        if dir.is_empty() {
            self.outputs_dir.clone()
        } else {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                self.data_dir.join(path)
            }
        }
    }

    fn handle_settings_get(&self) -> HttpResponse {
        let mut settings = match self.settings() {
            Ok(settings) => settings,
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        // The parse server only serves MinerU official (mineru.net);
        // 302.AI is not offered here. Force the effective provider for
        // every read below (key pool, base URL, desktop-key check) and
        // for the reported value. The stored value is left alone — the
        // desktop may still use 302 on its own.
        settings.mineru.provider = sag_protocol::mineru::MineruProvider::Official;
        let pool = KeyPool::new(
            self.data_dir.clone(),
            settings.mineru.provider,
            settings.parse.mineru_key_cooldown_secs,
            self.jobs.clone(),
        );
        let key_count = pool.pool().map(|p| p.len()).unwrap_or(0);
        // `settings.mineru.enabled` is recomputed by `Store::settings()`
        // from the desktop single-key slot on every load, so it cannot
        // carry a parse-server PUT value — report the effective switch
        // (explicit parse-server choice, else key-pool auto-enable).
        let desktop_has_key =
            crate::mineru::has_key(&self.data_dir, &settings.mineru).unwrap_or(false);
        let mineru_enabled =
            effective_mineru_enabled(settings.parse.mineru_enabled, key_count, desktop_has_key);
        json_response(
            StatusCode::OK,
            json!({
                "mineru": {
                    "enabled": mineru_enabled,
                    "mineru_enabled": settings.parse.mineru_enabled,
                    "provider": settings.mineru.provider,
                    "base_url": settings.mineru.base_url(),
                    "has_key": settings.mineru.has_key,
                    "key_count": key_count,
                    "request_timeout_secs": settings.mineru.request_timeout_secs,
                    "task_timeout_secs": settings.mineru.task_timeout_secs,
                    "poll_interval_secs": settings.mineru.poll_interval_secs,
                    "concurrency": settings.mineru.concurrency,
                    "key_cooldown_secs": settings.parse.mineru_key_cooldown_secs,
                    "model_version": settings.mineru.model_version,
                    "version": settings.mineru.version,
                    "parse_method": settings.mineru.parse_method,
                    "ocr": settings.mineru.ocr,
                    "table": settings.mineru.table,
                    "formula": settings.mineru.formula,
                },
                "engines": {
                    "pdf": settings.parse.pdf_engine,
                    "office": settings.parse.office_engine,
                    "image": settings.parse.image_engine,
                    "audio": settings.parse.audio_engine,
                    "video": settings.parse.video_engine,
                },
                "docling": {
                    "anydoc_timeout_secs": settings.runtime.anydoc_timeout_secs,
                    "docling_timeout_secs": settings.runtime.docling_timeout_secs,
                    "conversion_input_mib": settings.runtime.conversion_input_mib,
                    "conversion_output_mib": settings.runtime.conversion_output_mib,
                    "docling_threads": settings.runtime.docling_threads,
                    "office_text_only": settings.runtime.office_text_only,
                    "do_ocr": settings.runtime.do_ocr,
                    "do_table_structure": settings.runtime.do_table_structure,
                    "max_document_pages": settings.runtime.max_document_pages,
                },
                "files": {
                    "convert_other_formats": settings.convert_other_formats,
                    "conversion_formats": settings.conversion_formats,
                    "conversion_storage": settings.conversion_storage,
                    "output_dir": self.outputs_root(&settings).to_string_lossy(),
                },
                "queue": {
                    "queue_concurrency": settings.parse.queue_concurrency,
                    "mineru_concurrency": settings.mineru.concurrency,
                    "conversion_workers": settings.conversion_workers,
                },
                "asr": settings.parse.asr,
                "vision": {
                    "base_url": settings.parse.vision.base_url,
                    "model": settings.parse.vision.model,
                    "timeout_secs": settings.parse.vision.timeout_secs,
                    "has_key": crate::models::has_secret(&self.data_dir, "vision").unwrap_or(false),
                    "enabled": settings.parse.vision.enabled(),
                },
                "logs": {
                    "retention_days": settings.parse.log_retention_days,
                },
            }),
        )
    }

    async fn handle_settings_put(&self, req: Request<Incoming>) -> HttpResponse {
        let body = match self.read_body(req, 1024 * 1024).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(StatusCode::BAD_REQUEST, format!("JSON 无效：{error}"))
            }
        };
        let mut settings = match self.settings() {
            Ok(settings) => settings,
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        if let Some(mineru) = value.get("mineru") {
            // 302.AI is retired from the parse server: only MinerU
            // official is accepted. Reject it loudly instead of
            // silently storing a provider the server never uses.
            if let Some(provider) = mineru.get("provider").and_then(Value::as_str) {
                if provider == "302" || provider == "302_opensource" {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "302.AI 已移除：解析服务只支持 MinerU 官方",
                    );
                }
            }
            let mut next = settings.mineru.clone();
            if let Ok(patched) = serde_json::from_value::<sag_protocol::mineru::MineruConfig>(
                merge_json(serde_json::to_value(&next).unwrap(), mineru.clone()),
            ) {
                next = patched;
            } else {
                return error_response(StatusCode::BAD_REQUEST, "mineru 设置无效");
            }
            if let Err(error) = next.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            // MinerU stays; only the 302 endpoint is gone — a custom
            // base URL pointing at api.302ai.cn is rejected here too,
            // so the 302 address cannot be typed back into the config.
            if next.base_url().contains("302ai.cn") {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "接口地址不能用 302.AI（api.302ai.cn 已移除），请用 MinerU 官方地址或留空",
                );
            }
            settings.mineru = next;
            // Persist an explicit on/off choice in the parse-server
            // settings (`settings.parse.mineru_enabled`): the
            // `settings.mineru.enabled` value above is recomputed by
            // `Store::settings()` on the next load and would not survive.
            // A `mineru` patch without `enabled` leaves the stored
            // explicit choice untouched.
            if let Some(enabled) = mineru.get("enabled").and_then(Value::as_bool) {
                settings.parse.mineru_enabled = Some(enabled);
                settings.mineru.enabled = enabled;
            }
        }
        if let Some(runtime) = value.get("runtime").or_else(|| value.get("docling")) {
            let mut next = settings.runtime.clone();
            if let Ok(patched) = serde_json::from_value::<sag_protocol::runtime::RuntimeSettings>(
                merge_json(serde_json::to_value(&next).unwrap(), runtime.clone()),
            ) {
                next = patched;
            } else {
                return error_response(StatusCode::BAD_REQUEST, "runtime 设置无效");
            }
            if let Err(error) = next.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            settings.runtime = next;
        }
        if let Some(parse) = value.get("parse") {
            let mut next = settings.parse.clone();
            if let Ok(patched) =
                serde_json::from_value::<sag_protocol::parse_settings::ParseServerSettings>(
                    merge_json(serde_json::to_value(&next).unwrap(), parse.clone()),
                )
            {
                next = patched;
            } else {
                return error_response(StatusCode::BAD_REQUEST, "parse 设置无效");
            }
            if let Err(error) = next.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            settings.parse = next;
        }
        if let Some(engines) = value.get("engines") {
            // Per-file-type first engine + audio/video first engine.
            // Unknown values are rejected (never silently coerced to auto).
            let parse = &mut settings.parse;
            if let Some(v) = engines.get("pdf") {
                parse.pdf_engine = match serde_json::from_value(v.clone()) {
                    Ok(v) => v,
                    Err(_) => return error_response(StatusCode::BAD_REQUEST, "engines.pdf 无效"),
                };
            }
            if let Some(v) = engines.get("office") {
                parse.office_engine = match serde_json::from_value(v.clone()) {
                    Ok(v) => v,
                    Err(_) => return error_response(StatusCode::BAD_REQUEST, "engines.office 无效"),
                };
            }
            if let Some(v) = engines.get("image") {
                parse.image_engine = match serde_json::from_value(v.clone()) {
                    Ok(v) => v,
                    Err(_) => return error_response(StatusCode::BAD_REQUEST, "engines.image 无效"),
                };
            }
            if let Some(v) = engines.get("audio") {
                parse.audio_engine = match serde_json::from_value(v.clone()) {
                    Ok(v) => v,
                    Err(_) => return error_response(StatusCode::BAD_REQUEST, "engines.audio 无效"),
                };
            }
            if let Some(v) = engines.get("video") {
                parse.video_engine = match serde_json::from_value(v.clone()) {
                    Ok(v) => v,
                    Err(_) => return error_response(StatusCode::BAD_REQUEST, "engines.video 无效"),
                };
            }
            if let Err(error) = parse.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
        }
        if let Some(queue) = value.get("queue") {
            if let Some(concurrency) = queue.get("queue_concurrency").and_then(Value::as_u64) {
                settings.parse.queue_concurrency = concurrency as usize;
            }
            if let Some(concurrency) = queue.get("mineru_concurrency").and_then(Value::as_u64) {
                settings.mineru.concurrency = concurrency as usize;
            }
            if let Err(error) = settings
                .parse
                .validate()
                .and_then(|_| settings.mineru.validate().map_err(|e| e))
            {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
        }
        if let Some(files) = value.get("files") {
            if let Some(output_dir) = files.get("output_dir").and_then(Value::as_str) {
                settings.parse.output_dir = output_dir.to_string();
            }
            if let Some(formats) = files.get("conversion_formats") {
                settings.conversion_formats =
                    serde_json::from_value(formats.clone()).unwrap_or(settings.conversion_formats);
            }
        }
        if let Some(asr) = value.get("asr") {
            let mut next = settings.parse.asr.clone();
            if let Ok(patched) = serde_json::from_value::<sag_protocol::parse_settings::AsrConfig>(
                merge_json(serde_json::to_value(&next).unwrap(), asr.clone()),
            ) {
                next = patched;
            } else {
                return error_response(StatusCode::BAD_REQUEST, "asr 设置无效");
            }
            if let Err(error) = next.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            settings.parse.asr = next;
        }
        if let Some(vision) = value.get("vision") {
            // A `key` here is write-only: it is stored via the credentials
            // mechanism and never echoed back (only `has_key` is exposed).
            if let Some(key) = vision.get("key").and_then(Value::as_str) {
                if key.trim().is_empty() {
                    if let Err(error) = crate::models::clear_secret(&self.data_dir, "vision") {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            format!("清除视觉模型 Key 失败：{error:#}"),
                        );
                    }
                    settings.parse.vision.has_key = false;
                } else {
                    if let Err(error) =
                        crate::models::save_secret(&self.data_dir, "vision", key.trim())
                    {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            format!("保存视觉模型 Key 失败：{error:#}"),
                        );
                    }
                    settings.parse.vision.has_key = true;
                }
            }
            let mut next = settings.parse.vision.clone();
            if let Ok(patched) = serde_json::from_value::<sag_protocol::parse_settings::VisionConfig>(
                merge_json(serde_json::to_value(&next).unwrap(), vision.clone()),
            ) {
                next = patched;
            } else {
                return error_response(StatusCode::BAD_REQUEST, "vision 设置无效");
            }
            if let Err(error) = next.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            settings.parse.vision = next;
        }
        if let Some(logs) = value.get("logs") {
            if let Some(days) = logs.get("retention_days").and_then(Value::as_u64) {
                settings.parse.log_retention_days = days as u32;
            }
            if let Err(error) = settings.parse.validate() {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
        }
        match self.store.save_settings(&settings) {
            Ok(()) => {
                let _ = self.refresh_semaphore();
                let _ = self.jobs.prune_logs(settings.parse.log_retention_days);
                self.handle_settings_get()
            }
            Err(error) => {
                error_response(StatusCode::BAD_REQUEST, format!("保存设置失败：{error:#}"))
            }
        }
    }

    fn handle_keys_get(&self) -> HttpResponse {
        let settings = match self.settings() {
            Ok(settings) => settings,
            Err(error) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        };
        // Keys are always official-pool keys; 302.AI is retired here.
        let provider = sag_protocol::mineru::MineruProvider::Official;
        let pool = KeyPool::new(
            self.data_dir.clone(),
            provider,
            settings.parse.mineru_key_cooldown_secs,
            self.jobs.clone(),
        );
        match pool.masked_list() {
            Ok(keys) => json_response(
                StatusCode::OK,
                json!({"provider": keypool::provider_name(provider), "keys": keys}),
            ),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }

    async fn handle_keys_post(&self, req: Request<Incoming>) -> HttpResponse {
        let body = match self.read_body(req, 64 * 1024).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(StatusCode::BAD_REQUEST, format!("JSON 无效：{error}"))
            }
        };
        let Some(key) = value
            .get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.trim().is_empty())
        else {
            return error_response(StatusCode::BAD_REQUEST, "key 不能为空");
        };
        // Keys are always official-pool keys; a "302" provider is
        // rejected instead of stored under a slot the server never reads.
        if let Some(provider) = value.get("provider").and_then(Value::as_str) {
            if provider == "302" || provider == "302_opensource" {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "302.AI 已移除：解析服务只支持 MinerU 官方",
                );
            }
            if provider != "official" {
                return error_response(StatusCode::BAD_REQUEST, "provider 无效");
            }
        }
        let provider = sag_protocol::mineru::MineruProvider::Official;
        match keypool::save_pool_key(&self.data_dir, provider, key) {
            Ok(slot) => json_response(
                StatusCode::OK,
                json!({"index": slot, "masked": super::types::mask_key(key), "provider": keypool::provider_name(provider)}),
            ),
            Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
        }
    }

    fn handle_keys_delete(&self, path: &str, _query: &str) -> HttpResponse {
        let rest = path.trim_start_matches("/api/settings/mineru-keys");
        let index: i64 = match rest.trim_start_matches('/').parse() {
            Ok(index) => index,
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "key 序号无效"),
        };
        // Keys are always official-pool keys.
        match keypool::delete_pool_key(
            &self.data_dir,
            sag_protocol::mineru::MineruProvider::Official,
            index,
        ) {
            Ok(()) => json_response(StatusCode::OK, json!({"deleted": index})),
            Err(error) => error_response(StatusCode::BAD_REQUEST, error.to_string()),
        }
    }

    fn handle_logs(&self, query: &str) -> HttpResponse {
        let params: HashMap<String, String> = query
            .split('&')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((percent_decode(key), percent_decode(value)))
            })
            .collect();
        let level = params.get("level").map(String::as_str);
        let task_id = params.get("task_id").map(String::as_str);
        let limit = params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100)
            .clamp(1, 1000);
        match self.jobs.logs(level, task_id, limit) {
            Ok(logs) => json_response(StatusCode::OK, json!({"logs": logs})),
            Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }
}

/// Effective MinerU switch reported by `GET /api/settings` and honoured
/// by the job runner: an explicit parse-server choice
/// (`settings.parse.mineru_enabled`, set by `PUT /api/settings`) wins;
/// otherwise auto — on when any key exists (key pool or the desktop
/// single-key slot), matching the historic JobRunner auto-enable.
fn effective_mineru_enabled(
    explicit: Option<bool>,
    key_count: usize,
    desktop_has_key: bool,
) -> bool {
    explicit.unwrap_or(key_count > 0 || desktop_has_key)
}

/// One row of `GET /api/tasks`. Field names follow `ui/app.js` (`normTask`
/// reads `task_id`/`input_file`/`output_file`/`status`/`engine`/
/// `client_id`/`error`/`created_at`/`updated_at`/`progress`).
///
/// Two adaptations, because the stored record and the frontend disagree:
/// * timestamps are stored as Unix **seconds** but the frontend feeds
///   them straight into `new Date(...)` (milliseconds) and subtracts
///   them for durations, so they are emitted as **milliseconds** here.
///   (The single-task endpoint still returns raw seconds — unchanged.)
/// * `progress` is stored as a JSON string; it is emitted as the
///   `{current, total}` object the frontend's `progressOf` expects, so
///   the segment progress bar renders.
fn list_task_json(record: &ParseJobRecord) -> Value {
    let progress = record
        .progress
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| {
            let current = value.get("segment_current").and_then(Value::as_u64)?;
            let total = value.get("segment_total").and_then(Value::as_u64)?;
            Some(json!({"current": current, "total": total}))
        });
    json!({
        "task_id": record.task_id,
        "input_file": record.input_file,
        "output_file": record.output_file,
        "status": record.status.as_str(),
        "engine": record.engine,
        "client_id": record.client_id,
        "error": record.error,
        "created_at": record.created_at * 1000,
        "updated_at": record.updated_at * 1000,
        "progress": progress,
    })
}

/// Deep-merge a JSON patch object over a base object (shallow per key,
/// objects merged recursively).
fn merge_json(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut base), Value::Object(patch)) => {
            for (key, value) in patch {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, patch) => patch,
    }
}

/// Minimal percent-decoding for query strings (`+` → space, `%XX`).
fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Run the parse-only server until `shutdown` fires. Entry point used by
/// the CLI (`sag parse-server`).
pub async fn run_parse_server(
    data_dir: PathBuf,
    host: String,
    port: u16,
    shutdown: CancellationToken,
) -> Result<()> {
    let server = ParseServer::open(data_dir, host, port)?;
    server.serve(shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_server::types::JobStatus;

    #[test]
    fn effective_mineru_enabled_explicit_beats_auto() {
        // No explicit choice: auto — on when any key exists.
        assert!(!effective_mineru_enabled(None, 0, false));
        assert!(effective_mineru_enabled(None, 1, false));
        assert!(effective_mineru_enabled(None, 0, true));
        // Explicit choice is read back verbatim either way.
        assert!(effective_mineru_enabled(Some(true), 0, false));
        assert!(!effective_mineru_enabled(Some(false), 3, true));
    }

    #[test]
    fn list_task_json_uses_frontend_conventions() {
        let record = ParseJobRecord {
            task_id: "abc123".into(),
            input_md5: "abc123".into(),
            input_file: "ab/c1/report.pdf".into(),
            output_file: "temp/ab/c1/abc123.md".into(),
            client_id: Some("app-a".into()),
            engine: "mineru".into(),
            status: JobStatus::Processing,
            prompt: None,
            attempts: vec![],
            error: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_060,
            started_at: None,
            finished_at: None,
            pages: None,
            progress: Some(
                r#"{"engine":"MinerU","phase":null,"segment_current":2,"segment_total":5,"elapsed_ms":123}"#.into(),
            ),
        };
        let value = list_task_json(&record);
        assert_eq!(value["task_id"], "abc123");
        assert_eq!(value["status"], "processing");
        assert_eq!(value["client_id"], "app-a");
        // Unix seconds in the record become milliseconds for the
        // frontend's `new Date(...)` / duration arithmetic.
        assert_eq!(value["created_at"], 1_700_000_000_000i64);
        assert_eq!(value["updated_at"], 1_700_000_060_000i64);
        assert_eq!(value["progress"]["current"], 2);
        assert_eq!(value["progress"]["total"], 5);
        let no_progress = ParseJobRecord {
            progress: None,
            ..record
        };
        assert!(list_task_json(&no_progress)["progress"].is_null());
    }
}
