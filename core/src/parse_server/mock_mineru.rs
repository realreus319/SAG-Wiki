//! Local mock backends for parse-server tests. No real MinerU, ASR or
//! vision service is ever contacted: the mock speaks the same HTTP shapes
//! (MinerU v4 online flow, FunASR `/asr`, OpenAI-compatible
//! `/v1/audio/transcriptions`, VibeVoice submit/poll, OpenAI-compatible
//! `/v1/chat/completions` with `video_url`).
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Bytes, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, sync::oneshot};

type HttpResponse = Response<BoxBody<Bytes, std::convert::Infallible>>;

fn json_response(status: StatusCode, value: serde_json::Value) -> HttpResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(&value).unwrap())).boxed())
        .unwrap()
}

fn zip_with_full_md(markdown: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("full.md", options).unwrap();
        use std::io::Write;
        zip.write_all(markdown.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

/// Mock MinerU official (mineru.net v4) backend.
pub struct MockMineru {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    markdown: String,
}

impl MockMineru {
    pub async fn start(markdown: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let polls = Arc::new(Mutex::new(0u32));
        let markdown = markdown.to_string();
        let markdown_for_task = markdown.clone();
        tokio::spawn(async move {
            let polls = polls.clone();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let polls = polls.clone();
                        let markdown = markdown_for_task.clone();
                        let addr = addr;
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                                let polls = polls.clone();
                                let markdown = markdown.clone();
                                async move { Ok::<_, std::convert::Infallible>(mock_mineru_route(req, &polls, &markdown, addr).await) }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                }
            }
        });
        Self {
            addr,
            shutdown: Some(shutdown_tx),
            markdown,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn mock_mineru_route(
    req: Request<hyper::body::Incoming>,
    polls: &Arc<Mutex<u32>>,
    markdown: &str,
    addr: SocketAddr,
) -> HttpResponse {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if method == hyper::Method::POST && path == "/file-urls/batch" {
        return json_response(
            StatusCode::OK,
            serde_json::json!({"code": 0, "data": {"batch_id": "mock-batch-1", "file_urls": [format!("http://{addr}/upload")]}}),
        );
    }
    if method == hyper::Method::PUT && path == "/upload" {
        let _ = req.into_body().collect().await;
        return json_response(StatusCode::OK, serde_json::json!({"code": 0}));
    }
    if method == hyper::Method::GET && path == "/extract-results/batch/mock-batch-1" {
        let mut count = polls.lock().unwrap();
        *count += 1;
        let state = if *count >= 2 { "done" } else { "running" };
        let mut entry = serde_json::json!({"state": state});
        if state == "done" {
            entry["full_zip_url"] = serde_json::json!(format!("http://{addr}/result.zip"));
        }
        return json_response(
            StatusCode::OK,
            serde_json::json!({"code": 0, "data": {"extract_result": [entry]}}),
        );
    }
    if method == hyper::Method::GET && path == "/result.zip" {
        let zip = zip_with_full_md(markdown);
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/zip")
            .body(Full::new(Bytes::from(zip)).boxed())
            .unwrap();
    }
    json_response(
        StatusCode::NOT_FOUND,
        serde_json::json!({"error": "not found"}),
    )
}

/// Mock ASR + vision backend (FunASR, OpenAI ASR, VibeVoice, vision).
pub struct MockMediaBackend {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl MockMediaBackend {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(|req: Request<hyper::body::Incoming>| async move {
                                Ok::<_, std::convert::Infallible>(mock_media_route(req).await)
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                }
            }
        });
        Self {
            addr,
            shutdown: Some(shutdown_tx),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn mock_media_route(req: Request<hyper::body::Incoming>) -> HttpResponse {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    // Drain the body so the connection stays usable.
    let _ = req.into_body().collect().await;
    if method == hyper::Method::POST && path == "/asr" {
        return json_response(
            StatusCode::OK,
            serde_json::json!({"segments": [{"text": "hello funasr", "start": 0.0, "end": 1.5, "speaker": "SPK0"}]}),
        );
    }
    if method == hyper::Method::POST && path == "/v1/audio/transcriptions" {
        return json_response(StatusCode::OK, serde_json::json!({"text": "hello openai"}));
    }
    if method == hyper::Method::POST && path == "/submit/url" {
        return json_response(StatusCode::OK, serde_json::json!({"task_id": "vv-1"}));
    }
    if method == hyper::Method::GET && path == "/status/vv-1" {
        return json_response(
            StatusCode::OK,
            serde_json::json!({"task_status": "已完成", "text": "hello vibevoice"}),
        );
    }
    if method == hyper::Method::POST && path == "/v1/chat/completions" {
        return json_response(
            StatusCode::OK,
            serde_json::json!({"choices": [{"message": {"content": "# Vision summary\n\n[00:00] mock frame"}}]}),
        );
    }
    json_response(
        StatusCode::NOT_FOUND,
        serde_json::json!({"error": "not found"}),
    )
}
