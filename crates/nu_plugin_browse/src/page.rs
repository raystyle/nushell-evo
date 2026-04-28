use chaser_oxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFinished, EventLoadingFailed,
    EventRequestWillBeSent, EventResponseReceived, GetResponseBodyParams, ResourceType,
};
use chaser_oxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnableParams;
use chaser_oxide::cdp::js_protocol::runtime::EventExceptionThrown;
use chaser_oxide::ChaserPage;
use futures::StreamExt;
use nu_protocol::{Record, Span, Value};
use regex::Regex;
use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use crate::utils::{wrap_eval_js, check_eval_result};

#[allow(clippy::too_many_arguments)]
pub async fn page_navigate(
    chaser: &ChaserPage,
    url: &str,
    stealth: bool,
    wait: Option<Duration>,
    init_script: Option<&str>,
    eval_js: Option<&str>,
    real_eval: bool,
    ntrace: Option<(bool, bool, Option<Regex>)>,
    span: Span,
) -> Result<(Option<String>, Option<Vec<Record>>, Vec<String>), Box<dyn Error>> {
    if let Some(path) = init_script {
        let js = fs::read_to_string(path)?;
        chaser.raw_page().add_init_script(js).await?;
    }

    if stealth {
        chaser.apply_native_profile().await?;
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let network_log: Arc<Mutex<Vec<Record>>> = Arc::new(Mutex::new(Vec::new()));
    let init_exceptions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Event-driven network idle: track pending request count via CDP events.
    let pending_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    let all_done: Arc<Notify> = Arc::new(Notify::new());

    if init_script.is_some() {
        let _ = chaser
            .raw_page()
            .execute(RuntimeEnableParams::default())
            .await;

        let exc_log = init_exceptions.clone();
        let stop = stop_flag.clone();
        let mut exc_events = chaser
            .raw_page()
            .event_listener::<EventExceptionThrown>()
            .await?;
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(200), exc_events.next()).await {
                    Ok(Some(event)) => {
                        let details = &event.exception_details;
                        let desc = details
                            .exception
                            .as_ref()
                            .and_then(|e| e.description.as_deref())
                            .unwrap_or(&details.text);
                        let msg = format!(
                            "{}:{}: {}",
                            details.line_number, details.column_number, desc
                        );
                        exc_log.lock().unwrap().push(msg);
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }

    // Always enable CDP Network domain for event-driven idle detection.
    let enable_params = NetworkEnableParams {
        max_total_buffer_size: Some(50_000_000),
        max_resource_buffer_size: Some(5_000_000),
        ..Default::default()
    };
    let _ = chaser.raw_page().execute(enable_params).await;

    let show_req = ntrace.as_ref().is_some_and(|(r, _, _)| *r);
    let show_res = ntrace.as_ref().is_some_and(|(_, r, _)| *r);
    let filter = ntrace.as_ref().and_then(|(_, _, f)| f.clone());
    let filter_res = filter.clone();

    // Single RequestWillBeSent listener: handles both ntrace recording and pending count.
    {
        let log = network_log.clone();
        let stop = stop_flag.clone();
        let pending = pending_count.clone();
        let listener_span = span;
        let mut events = chaser
            .raw_page()
            .event_listener::<EventRequestWillBeSent>()
            .await?;
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(200), events.next()).await {
                    Ok(Some(event)) => {
                        // Skip persistent connections (SSE, WebSocket) — they never finish.
                        let is_persistent = matches!(
                            event.r#type,
                            Some(ResourceType::EventSource) | Some(ResourceType::WebSocket)
                        );
                        if !is_persistent {
                            pending.fetch_add(1, Ordering::Relaxed);
                        }
                        if show_req && filter
                            .as_ref()
                            .is_none_or(|r| r.is_match(&event.request.url))
                        {
                            let mut rec = Record::new();
                            rec.push("type", Value::string("request", listener_span));
                            rec.push(
                                "method",
                                Value::string(&event.request.method, listener_span),
                            );
                            rec.push("url", Value::string(&event.request.url, listener_span));
                            rec.push(
                                "headers",
                                Value::string(
                                    event.request.headers.inner().to_string(),
                                    listener_span,
                                ),
                            );
                            log.lock().unwrap().push(rec);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }

    if show_res {
        let log = network_log.clone();
        let stop = stop_flag.clone();
        let listener_span = span;
        let mut events = chaser
            .raw_page()
            .event_listener::<EventResponseReceived>()
            .await?;
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(200), events.next()).await {
                    Ok(Some(event)) => {
                        if filter_res
                            .as_ref()
                            .is_none_or(|r| r.is_match(&event.response.url))
                        {
                            let mut rec = Record::new();
                            rec.push("type", Value::string("response", listener_span));
                            rec.push(
                                "id",
                                Value::string(event.request_id.as_ref(), listener_span),
                            );
                            rec.push(
                                "status",
                                Value::int(event.response.status, listener_span),
                            );
                            rec.push("url", Value::string(&event.response.url, listener_span));
                            rec.push(
                                "mime",
                                Value::string(&event.response.mime_type, listener_span),
                            );
                            rec.push(
                                "headers",
                                Value::string(
                                    event.response.headers.inner().to_string(),
                                    listener_span,
                                ),
                            );
                            log.lock().unwrap().push(rec);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }

    // LoadingFinished / LoadingFailed → decrement pending count, notify when idle.
    {
        let pending = pending_count.clone();
        let done = all_done.clone();
        let stop = stop_flag.clone();
        let mut load_events = chaser
            .raw_page()
            .event_listener::<EventLoadingFinished>()
            .await?;
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(200), load_events.next()).await {
                    Ok(Some(_)) => {
                        let prev = pending.fetch_sub(1, Ordering::Relaxed);
                        if prev == 1 {
                            done.notify_one();
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }

    {
        let pending = pending_count.clone();
        let done = all_done.clone();
        let stop = stop_flag.clone();
        let mut fail_events = chaser
            .raw_page()
            .event_listener::<EventLoadingFailed>()
            .await?;
        tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(200), fail_events.next()).await {
                    Ok(Some(_)) => {
                        let prev = pending.fetch_sub(1, Ordering::Relaxed);
                        if prev == 1 {
                            done.notify_one();
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
        });
    }

    chaser.goto(url).await?;

    if let Some(d) = wait {
        tokio::time::sleep(d).await;
    }

    // Event-driven idle: wait for all regular (non-persistent) requests to finish.
    // Persistent connections (SSE, WebSocket) are excluded from pending count.
    // 300ms debounce after pending reaches 0 to catch cascading requests.
    // 10s hard timeout as safety net.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if pending_count.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if pending_count.load(Ordering::Relaxed) == 0 {
                    break;
                }
            } else {
                let _ = all_done.notified().await;
            }
        }
    })
    .await.ok();

    stop_flag.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(250)).await;

    let init_errors: Vec<String> = {
        let mut guard = init_exceptions.lock().unwrap();
        std::mem::take(&mut *guard)
    };

    let network_records = if ntrace.is_some() {
        let mut log = match Arc::try_unwrap(network_log) {
            Ok(mutex) => mutex.into_inner().unwrap(),
            Err(arc) => arc.lock().unwrap().clone(),
        };

        for rec in &mut log {
            if rec.get("type").and_then(|v| v.as_str().ok()) != Some("response") {
                continue;
            }
            let Some(id) = rec.get("id").and_then(|v| v.as_str().ok()) else {
                continue;
            };
            let params = GetResponseBodyParams::new(id.to_string());
            if let Ok(result) = chaser.raw_page().execute(params).await {
                rec.push("body", Value::string(result.result.body, span));
            }
        }

        if log.is_empty() { None } else { Some(log) }
    } else {
        None
    };

    let content = if let Some(js) = eval_js {
        let wrapped = wrap_eval_js(js);
        if real_eval {
            let result = chaser
                .raw_page()
                .evaluate(wrapped.as_str())
                .await
                .map_err(|e| -> Box<dyn Error> { format!("{e}").into() })?;
            let s = match result.value() {
                Some(v) => match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                },
                None => String::new(),
            };
            match check_eval_result(&s) {
                Ok(val) => Some(val),
                Err(err) => return Err(format!("eval error: {err}").into()),
            }
        } else {
            let result = chaser.evaluate(wrapped.as_str()).await?;
            match result {
                Some(v) => {
                    let s = match v.as_str() {
                        Some(s) => s.to_string(),
                        None => v.to_string(),
                    };
                    match check_eval_result(&s) {
                        Ok(val) => Some(val),
                        Err(err) => return Err(format!("eval error: {err}").into()),
                    }
                }
                None => {
                    return Err("eval error: JavaScript execution failed (no return value)".into());
                }
            }
        }
    } else {
        let html = chaser.content().await?;
        Some(html)
    };

    Ok((content, network_records, init_errors))
}

pub async fn page_eval_only(
    chaser: &ChaserPage,
    js: &str,
    real_eval: bool,
) -> Result<String, Box<dyn Error>> {
    // For eval-only mode, just run the JS without network idle wait.
    // The persistent browser session handles timing externally.
    let wrapped = wrap_eval_js(js);
    if real_eval {
        let result = chaser
            .raw_page()
            .evaluate(wrapped.as_str())
            .await
            .map_err(|e| -> Box<dyn Error> { format!("{e}").into() })?;
        let s = match result.value() {
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            },
            None => return Err("eval error: JavaScript execution failed (no return value)".into()),
        };
        check_eval_result(&s).map_err(|e| -> Box<dyn Error> { format!("eval error: {e}").into() })
    } else {
        let result = chaser.evaluate(wrapped.as_str()).await?;
        match result {
            Some(v) => {
                let s = match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                };
                check_eval_result(&s)
                    .map_err(|e| -> Box<dyn Error> { format!("eval error: {e}").into() })
            }
            None => Err("eval error: JavaScript execution failed (no return value)".into()),
        }
    }
}
