use chaser_oxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFinished, EventRequestWillBeSent,
    EventResponseReceived, GetResponseBodyParams,
};
use chaser_oxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnableParams;
use chaser_oxide::cdp::js_protocol::runtime::EventExceptionThrown;
use chaser_oxide::ChaserPage;
use futures::StreamExt;
use nu_protocol::{Record, Span, Value};
use regex::Regex;
use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::utils::{wrap_eval_js, check_eval_result};

const NETWORK_IDLE_JS: &str = r#"() =>
  new Promise((resolve) => {
    let activeRequests = 0;
    let idleTimer;

    const done = (label) => {
      clearTimeout(idleTimer);
      idleTimer = setTimeout(() => resolve(`${label}-network-idle`), 500);
    };

    const origOpen = XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open = function (...args) {
      this.addEventListener('loadstart', () => {
        activeRequests++;
        clearTimeout(idleTimer);
      });
      this.addEventListener('loadend', () => {
        activeRequests--;
        if (activeRequests <= 0) done('xhr');
      });
      origOpen.apply(this, args);
    };

    const origFetch = window.fetch;
    window.fetch = async function (...args) {
      activeRequests++;
      clearTimeout(idleTimer);
      try {
        const response = await origFetch.apply(this, args);
        return response;
      } finally {
        activeRequests--;
        if (activeRequests <= 0) done('fetch');
      }
    };

    const maybeResolveImmediately = () => {
      if (document.readyState === 'complete' && activeRequests === 0) {
        done('initial');
      } else {
        window.addEventListener('load', () => done('load'), { once: true });
      }
    };

    maybeResolveImmediately();
  })"#;

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
                    _ => break,
                }
            }
        });
    }

    if let Some((show_req, show_res, filter)) = &ntrace {
        let enable_params = NetworkEnableParams {
            max_total_buffer_size: Some(50_000_000),
            max_resource_buffer_size: Some(5_000_000),
            ..Default::default()
        };
        let _ = chaser.raw_page().execute(enable_params).await;

        if *show_req {
            let log = network_log.clone();
            let stop = stop_flag.clone();
            let filter = filter.clone();
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
                            if filter
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
                        _ => break,
                    }
                }
            });
        }

        if *show_res {
            let log = network_log.clone();
            let stop = stop_flag.clone();
            let filter = filter.clone();
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
                            if filter
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
                        _ => break,
                    }
                }
            });

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
                    match tokio::time::timeout(Duration::from_millis(200), load_events.next()).await
                    {
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
            });
        }
    }

    chaser.goto(url).await?;

    if let Some(d) = wait {
        tokio::time::sleep(d).await;
    }

    chaser.evaluate(NETWORK_IDLE_JS).await?;

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
    chaser.evaluate(NETWORK_IDLE_JS).await?;
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
