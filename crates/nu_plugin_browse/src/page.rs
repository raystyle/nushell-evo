use chaser_oxide::ChaserPage;
use chaser_oxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
    EventRequestWillBeSent, EventResponseReceived, GetResponseBodyParams, ResourceType,
};
use chaser_oxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnableParams;
use chaser_oxide::cdp::js_protocol::runtime::EventExceptionThrown;
use futures::StreamExt;
use nu_protocol::{Record, Span, Value};
use regex::Regex;
use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use crate::utils::{check_eval_result, wrap_eval_js};

// ---------------------------------------------------------------------------
// Timing constants
// ---------------------------------------------------------------------------

/// Interval for polling CDP event streams in spawned listener tasks.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Extra wait after pending request count reaches zero to catch cascading loads.
const NETWORK_IDLE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Hard upper bound for network-idle detection before giving up.
const NAVIGATE_HARD_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace period after setting the stop flag so spawned listeners can drain.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// NavigateParams
// ---------------------------------------------------------------------------

/// Collected parameters for [`page_navigate`], reducing the 10-argument signature
/// to a single struct.
pub struct NavigateParams<'a> {
    pub url: &'a str,
    pub stealth: bool,
    pub wait: Option<Duration>,
    pub init_script: Option<&'a str>,
    pub eval_js: Option<&'a str>,
    pub real_eval: bool,
    pub ntrace: Option<(bool, bool, Option<Regex>)>,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Event-listener helper
// ---------------------------------------------------------------------------

/// Spawn a tokio task that drains a CDP event stream until it ends, a timeout
/// fires, or the shared `stop` flag is set.
///
/// This replaces the five hand-rolled `loop { timeout + next }` patterns that
/// were previously duplicated throughout [`page_navigate`].
fn spawn_listener<T, F>(
    events: impl StreamExt<Item = T> + Unpin + Send + 'static,
    stop: Arc<AtomicBool>,
    mut on_event: F,
) where
    F: FnMut(T) + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let mut events = events;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match tokio::time::timeout(EVENT_POLL_INTERVAL, events.next()).await {
                Ok(Some(event)) => on_event(event),
                Ok(None) => break,
                Err(_) => {}
            }
        }
    });
}

// ---------------------------------------------------------------------------
// page_navigate
// ---------------------------------------------------------------------------

pub async fn page_navigate(
    chaser: &ChaserPage,
    params: &NavigateParams<'_>,
) -> Result<(Option<String>, Option<Vec<Record>>, Vec<String>), Box<dyn Error>> {
    let NavigateParams {
        url,
        stealth,
        wait,
        init_script,
        eval_js,
        real_eval,
        ntrace,
        span,
    } = params;

    if let Some(path) = init_script {
        let js = fs::read_to_string(path)?;
        chaser.raw_page().add_init_script(js).await?;
    }

    if *stealth {
        chaser.apply_native_profile().await?;
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let network_log: Arc<Mutex<Vec<Record>>> = Arc::new(Mutex::new(Vec::new()));
    let init_exceptions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Event-driven network idle: track pending request count via CDP events.
    let pending_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    let all_done: Arc<Notify> = Arc::new(Notify::new());

    // --- Init-script exception listener ---
    if init_script.is_some() {
        let _ = chaser
            .raw_page()
            .execute(RuntimeEnableParams::default())
            .await;

        let exc_log = init_exceptions.clone();
        let stop = stop_flag.clone();
        spawn_listener(
            chaser
                .raw_page()
                .event_listener::<EventExceptionThrown>()
                .await?,
            stop,
            move |event| {
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
                exc_log.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
            },
        );
    }

    // --- Network domain (always enabled for idle detection) ---
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

    // --- Request listener (ntrace + pending count) ---
    {
        let log = network_log.clone();
        let stop = stop_flag.clone();
        let pending = pending_count.clone();
        let listener_span = *span;
        spawn_listener(
            chaser
                .raw_page()
                .event_listener::<EventRequestWillBeSent>()
                .await?,
            stop,
            move |event| {
                // Skip persistent connections (SSE, WebSocket) — they never finish.
                let is_persistent = matches!(
                    event.r#type,
                    Some(ResourceType::EventSource) | Some(ResourceType::WebSocket)
                );
                if !is_persistent {
                    pending.fetch_add(1, Ordering::Relaxed);
                }
                if show_req
                    && filter
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
                        Value::string(event.request.headers.inner().to_string(), listener_span),
                    );
                    log.lock().unwrap_or_else(|e| e.into_inner()).push(rec);
                }
            },
        );
    }

    // --- Response listener ---
    if show_res {
        let log = network_log.clone();
        let stop = stop_flag.clone();
        let listener_span = *span;
        spawn_listener(
            chaser
                .raw_page()
                .event_listener::<EventResponseReceived>()
                .await?,
            stop,
            move |event| {
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
                    rec.push("status", Value::int(event.response.status, listener_span));
                    rec.push("url", Value::string(&event.response.url, listener_span));
                    rec.push(
                        "mime",
                        Value::string(&event.response.mime_type, listener_span),
                    );
                    rec.push(
                        "headers",
                        Value::string(event.response.headers.inner().to_string(), listener_span),
                    );
                    log.lock().unwrap_or_else(|e| e.into_inner()).push(rec);
                }
            },
        );
    }

    // --- LoadingFinished / LoadingFailed → decrement pending count ---
    // Both event types share the same handler: decrement the counter and
    // notify when it reaches zero.
    {
        let pending = pending_count.clone();
        let done = all_done.clone();
        let stop = stop_flag.clone();
        spawn_listener(
            chaser
                .raw_page()
                .event_listener::<EventLoadingFinished>()
                .await?,
            stop,
            move |_| {
                let prev = pending.fetch_sub(1, Ordering::Relaxed);
                if prev == 1 {
                    done.notify_one();
                }
            },
        );
    }
    {
        let pending = pending_count.clone();
        let done = all_done.clone();
        let stop = stop_flag.clone();
        spawn_listener(
            chaser
                .raw_page()
                .event_listener::<EventLoadingFailed>()
                .await?,
            stop,
            move |_| {
                let prev = pending.fetch_sub(1, Ordering::Relaxed);
                if prev == 1 {
                    done.notify_one();
                }
            },
        );
    }

    chaser.goto(url).await?;

    if let Some(d) = wait {
        tokio::time::sleep(*d).await;
    }

    // Event-driven idle: wait for all regular (non-persistent) requests to finish.
    // Persistent connections (SSE, WebSocket) are excluded from pending count.
    // NETWORK_IDLE_DEBOUNCE after pending reaches 0 to catch cascading requests.
    // NAVIGATE_HARD_TIMEOUT as safety net.
    tokio::time::timeout(NAVIGATE_HARD_TIMEOUT, async {
        loop {
            if pending_count.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(NETWORK_IDLE_DEBOUNCE).await;
                if pending_count.load(Ordering::Relaxed) == 0 {
                    break;
                }
            } else {
                let _ = all_done.notified().await;
            }
        }
    })
    .await
    .ok();

    stop_flag.store(true, Ordering::Relaxed);
    tokio::time::sleep(SHUTDOWN_GRACE).await;

    let init_errors: Vec<String> = {
        let mut guard = init_exceptions.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };

    let network_records = if ntrace.is_some() {
        let mut log = match Arc::try_unwrap(network_log) {
            Ok(mutex) => mutex.into_inner().unwrap(),
            Err(arc) => arc.lock().unwrap_or_else(|e| e.into_inner()).clone(),
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
                rec.push("body", Value::string(result.result.body, *span));
            }
        }

        if log.is_empty() { None } else { Some(log) }
    } else {
        None
    };

    let content = if let Some(js) = eval_js {
        let wrapped = wrap_eval_js(js);
        if *real_eval {
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
