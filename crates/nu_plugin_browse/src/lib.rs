use chaser_oxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFinished, EventRequestWillBeSent,
    EventResponseReceived, GetResponseBodyParams,
};
use chaser_oxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventExceptionThrown,
};
use chaser_oxide::{Browser, BrowserConfig, ChaserPage, Page, handler::viewport::Viewport};
use futures::StreamExt;
use nu_plugin::{EngineInterface, EvaluatedCall, Plugin, SimplePluginCommand};
use nu_protocol::{Category, Example, LabeledError, Record, Signature, Span, SyntaxShape, Value};
use regex::Regex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{error::Error, fs, path::PathBuf, time::Duration};

const EVAL_ERROR_PREFIX: &str = "__NU_EVAL_ERROR:";

const DEFAULT_DEBUG_PORT: u32 = 9223;
const PROFILE_DIR: &str = ".nu_browse_profile";
const SESSION_FILE: &str = ".session";

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

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct BrowsePlugin;

impl Plugin for BrowsePlugin {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    fn commands(&self) -> Vec<Box<dyn nu_plugin::PluginCommand<Plugin = Self>>> {
        vec![
            Box::new(Browse),
            Box::new(BrowseOpen),
            Box::new(BrowseClose),
        ]
    }
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

fn profile_dir(cwd: &str) -> PathBuf {
    PathBuf::from(cwd).join(PROFILE_DIR)
}

fn session_file(cwd: &str) -> PathBuf {
    profile_dir(cwd).join(SESSION_FILE)
}

fn has_active_session(cwd: &str) -> bool {
    session_file(cwd).exists()
}

fn load_ws_url(cwd: &str) -> Option<String> {
    fs::read_to_string(session_file(cwd))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_session(cwd: &str, ws_url: &str) -> std::io::Result<()> {
    let dir = profile_dir(cwd);
    fs::create_dir_all(&dir)?;
    fs::write(session_file(cwd), ws_url)
}

fn clear_session(cwd: &str) -> std::io::Result<()> {
    if session_file(cwd).exists() {
        fs::remove_file(session_file(cwd))
    } else {
        Ok(())
    }
}

async fn try_close_existing(cwd: &str) {
    if let Some(ws_url) = load_ws_url(cwd)
        && let Ok((mut browser, mut handler)) = Browser::connect(&ws_url).await
    {
        let _handle = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let _ = browser.close().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = clear_session(cwd);
}

// ---------------------------------------------------------------------------
// Shared: resolve eval_js and mode (real-eval vs isolated) from flag or pipeline input
// ---------------------------------------------------------------------------

fn resolve_eval_js_and_mode(call: &EvaluatedCall, input: &Value) -> Option<(String, bool)> {
    // Returns (js_code, is_real_eval)
    if let Some(js) = call.get_flag::<String>("real-eval").ok().flatten() {
        return Some((js, true));
    }
    if let Some(js) = call.get_flag::<String>("eval").ok().flatten() {
        return Some((js, false));
    }
    if !matches!(input, Value::Nothing { .. })
        && let Ok(js) = input.clone().coerce_into_string()
    {
        return Some((js, false));
    }
    None
}

fn parse_ntrace(value: &str) -> (bool, bool, Option<Regex>) {
    if let Some((mode, pattern)) = value.split_once(':') {
        let regex = Regex::new(pattern).ok();
        match mode {
            "request" => (true, false, regex),
            "response" => (false, true, regex),
            _ => (true, true, regex),
        }
    } else if value == "request" {
        (true, false, None)
    } else if value == "response" {
        (false, true, None)
    } else {
        let regex = Regex::new(value).ok();
        (true, true, regex)
    }
}

fn wrap_eval_js(js: &str) -> String {
    format!(
        "(function() {{ try {{ var __r = ({}); return __r === undefined ? null : JSON.stringify(__r); }} catch(__e) {{ return '{PREFIX}' + __e.toString(); }} }})()",
        js,
        PREFIX = EVAL_ERROR_PREFIX
    )
}

fn check_eval_result(s: &str) -> Result<String, String> {
    if let Some(stripped) = s.strip_prefix(EVAL_ERROR_PREFIX) {
        Err(stripped.to_string())
    } else {
        Ok(s.to_string())
    }
}

fn ensure_url(url: &str) -> Result<String, String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err(format!("invalid URL '{url}': must start with http:// or https://"))
    }
}

// ---------------------------------------------------------------------------
// Shared: navigate, wait, eval, extract
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn page_navigate(
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

    // Listen for Runtime.exceptionThrown (captures init-script errors)
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

async fn page_eval_only(
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

// ---------------------------------------------------------------------------
// Launch helpers
// ---------------------------------------------------------------------------

fn viewport_config(with_head: bool) -> Option<Viewport> {
    if with_head {
        None
    } else {
        Some(Viewport {
            width: 1920,
            height: 1080,
            device_scale_factor: Some(1.0),
            emulating_mobile: false,
            has_touch: false,
            is_landscape: false,
        })
    }
}

async fn launch_persistent(cwd: &str) -> Result<(Browser, Page), Box<dyn Error>> {
    let dir = profile_dir(cwd);
    fs::create_dir_all(&dir)?;

    let config = BrowserConfig::builder()
        .port(DEFAULT_DEBUG_PORT as u16)
        .user_data_dir(&dir)
        .window_size(1920, 1080)
        .viewport(None)
        .with_head()
        .arg("--test-type")
        .build()?;

    let (mut browser, mut handler) = Browser::launch(config).await?;

    let ws_url = format!("http://localhost:{}/json/version", DEFAULT_DEBUG_PORT);
    save_session(cwd, &ws_url)?;

    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // Reuse Chrome's default tab; keep extra tabs alive until after goto
    // so Chrome doesn't auto-create a new NTP to replace the navigated one
    let page = first_page(&mut browser).await?;
    Ok((browser, page))
}

async fn launch_ephemeral(with_head: bool) -> Result<(Browser, Page), Box<dyn Error>> {
    let mut config = BrowserConfig::builder()
        .window_size(1920, 1080)
        .viewport(viewport_config(with_head))
        .arg("--test-type");

    if with_head {
        config = config.with_head();
    } else {
        config = config.new_headless_mode();
    }

    let (mut browser, mut handler) = Browser::launch(config.build()?).await?;

    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // Reuse Chrome's default tab instead of creating a new one
    let page = first_page(&mut browser).await?;
    Ok((browser, page))
}

async fn first_page(browser: &mut Browser) -> Result<Page, Box<dyn Error>> {
    // Wait for Chrome's default tab to register after launch
    tokio::time::sleep(Duration::from_millis(500)).await;
    match browser.pages().await {
        Ok(pages) if !pages.is_empty() => {
            let mut pages = pages.into_iter();
            let page = pages.next().unwrap();
            for other in pages {
                let _ = other.close().await;
            }
            Ok(page)
        }
        _ => browser.new_page("about:blank").await.map_err(Into::into),
    }
}

// ---------------------------------------------------------------------------
// Command: browse
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Browse;

impl SimplePluginCommand for Browse {
    type Plugin = BrowsePlugin;

    fn name(&self) -> &str {
        "browse"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse")
            .optional("url", SyntaxShape::String, "The URL to browse")
            .switch("open", "Use persistent browser (same as 'browse open')", None)
            .switch("no-stealth", "Disable stealth mode", None)
            .switch("with-head", "Show browser window", None)
            .named(
                "init-script",
                SyntaxShape::Filepath,
                "JS file to inject before page scripts",
                None,
            )
            .named(
                "eval",
                SyntaxShape::String,
                "JS to execute after page load in isolated world (also accepts pipeline input)",
                Some('e'),
            )
            .named(
                "real-eval",
                SyntaxShape::String,
                "JS to execute after page load in the main world (mutually exclusive with --eval and pipeline input)",
                None,
            )
            .named(
                "wait",
                SyntaxShape::Duration,
                "Time to wait before retrieving html",
                Some('w'),
            )
            .named(
                "ntrace",
                SyntaxShape::String,
                "Trace network requests/responses (request, response, or regex pattern)",
                None,
            )
            .category(Category::Network)
    }

    fn description(&self) -> &str {
        "Browse a web page using a headless browser."
    }

    fn extra_description(&self) -> &str {
        "Without --open: launches a temporary browser that is closed after the request. \
         Refuses to run if a persistent browser is active (see 'browse open'). \
         With --open: delegates to persistent browser (same as 'browse open'). \
         Requires chrome/chromium installed."
    }

    fn examples(&'_ self) -> Vec<Example<'_>> {
        vec![
            Example {
                description: "Fetch a page with an ephemeral browser",
                example: "browse https://example.com",
                result: None,
            },
            Example {
                description: "Fetch with custom JS evaluation",
                example: "browse https://example.com --eval \"document.title\"",
                result: None,
            },
            Example {
                description: "Fetch with JS in the main world",
                example: "browse https://example.com --real-eval \"window.location.href\"",
                result: None,
            },
            Example {
                description: "Fetch with pipeline JS input",
                example: "\"document.title\" | browse https://example.com --eval $in",
                result: None,
            },
            Example {
                description: "Inject script before page loads",
                example: "browse https://example.com --init-script ./hook.js",
                result: None,
            },
            Example {
                description: "Open a persistent browser",
                example: "browse https://example.com --open",
                result: None,
            },
            Example {
                description: "Open or connect to existing persistent browser",
                example: "browse --open",
                result: None,
            },
        ]
    }

    fn run(
        &self,
        _plugin: &BrowsePlugin,
        _engine: &EngineInterface,
        call: &EvaluatedCall,
        input: &Value,
    ) -> Result<Value, LabeledError> {
        let use_open = call.has_flag("open")?;

        let url: Option<String> = call.opt(0)?;
        let stealth = !call.has_flag("no-stealth")?;
        let with_head = call.has_flag("with-head")?;
        let wait = call.get_flag::<Duration>("wait")?;
        let init_script: Option<String> = call.get_flag("init-script")?;

        let eval_info = resolve_eval_js_and_mode(call, input);
        let (eval_js, real_eval) = match eval_info {
            Some((js, re)) => (Some(js), re),
            None => (None, false),
        };
        let ntrace_str: Option<String> = call.get_flag("ntrace")?;
        let ntrace_opt = ntrace_str.as_deref().map(parse_ntrace);

        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            LabeledError::new(format!("{e}")).with_label("browse failed", call.head)
        })?;

        if use_open {
            // ---- Persistent browser logic (same as browse open) ----
            rt.block_on(async move {
                let session_path = session_file(&cwd).to_string_lossy().into_owned();
                let profile_path = profile_dir(&cwd).to_string_lossy().into_owned();

                let mut record = Record::new();
                record.push("session", Value::string(&session_path, call.head));
                record.push("port", Value::int(DEFAULT_DEBUG_PORT as i64, call.head));

                match (url, eval_js) {
                    (Some(url), eval_js) => {
                        if let Err(msg) = ensure_url(&url) {
                            record.push("status", Value::string("error", call.head));
                            record.push("url", Value::string(&url, call.head));
                            record.push("message", Value::string(msg, call.head));
                            return Ok(Value::record(record, call.head));
                        }
                        // Destroy old session if exists, start fresh
                        try_close_existing(&cwd).await;

                        let (browser, page) = launch_persistent(&cwd).await.map_err(|e| {
                            LabeledError::new(format!("{e}")).with_label("browse open failed", call.head)
                        })?;

                        let chaser = ChaserPage::new(page);
                        let nav_result = page_navigate(
                            &chaser, &url, stealth, wait,
                            init_script.as_deref(),
                            eval_js.as_deref(),
                            real_eval,
                            ntrace_opt,
                            call.head,
                        )
                        .await;

                        let (content, network_records, init_errors) = match nav_result {
                            Ok(r) => r,
                            Err(e) => {
                                let err_msg = e.to_string();
                                if err_msg.starts_with("eval error: ") {
                                    record.push("status", Value::string("error", call.head));
                                    record.push("url", Value::string(&url, call.head));
                                    record.push("message", Value::string(err_msg, call.head));
                                    return Ok(Value::record(record, call.head));
                                }
                                return Err(LabeledError::new(err_msg).with_label("browse open failed", call.head));
                            }
                        };


                        record.push("status", Value::string("opened", call.head));
                        record.push("url", Value::string(&url, call.head));
                        record.push("profile", Value::string(&profile_path, call.head));
                        if eval_js.is_some() {
                            record.push("eval", Value::string(content.unwrap_or_default(), call.head));
                        }
                        if let Some(net) = network_records {
                            record.push("network", Value::list(
                                net.into_iter().map(|r| Value::record(r, call.head)).collect(),
                                call.head,
                            ));
                        }
                        if !init_errors.is_empty() {
                            record.push("init_errors", Value::list(
                                init_errors.into_iter().map(|e| Value::string(e, call.head)).collect(),
                                call.head,
                            ));
                        }

                        std::mem::forget(browser);
                    }
                    (None, Some(js)) => {
                        // Eval on current page — requires existing session
                        if !has_active_session(&cwd) {
                            record.push("status", Value::string("error", call.head));
                            record.push("url", Value::string("", call.head));
                            record.push("message", Value::string(
                                "No active browser. Open a URL first with 'browse open <url>'.",
                                call.head,
                            ));
                            return Ok(Value::record(record, call.head));
                        }

                        let ws_url = load_ws_url(&cwd).ok_or_else(|| {
                            LabeledError::new("no session url").with_label("browse open failed", call.head)
                        })?;
                        let (mut browser, mut handler) = Browser::connect(&ws_url).await.map_err(|e| {
                            LabeledError::new(format!("{e}")).with_label("browse open failed", call.head)
                        })?;
                        let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

                        let _ = browser.fetch_targets().await;
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        let page = {
                            let all_pages = browser.pages().await.unwrap_or_default();
                            let mut found = None;
                            for p in &all_pages {
                                if let Ok(Some(purl)) = p.url().await
                                    && !purl.starts_with("chrome://")
                                    && !purl.starts_with("devtools://")
                                    && !purl.starts_with("about:blank")
                                {
                                    found = Some(p.clone());
                                    break;
                                }
                            }
                            found
                        };

                        match page {
                            Some(page) => {
                                let chaser = ChaserPage::new(page);
                                let current_url = chaser.url().await.unwrap_or_default().unwrap_or_default();
                                if current_url.is_empty() || current_url == "about:blank" {
                                    record.push("status", Value::string("error", call.head));
                                    record.push("url", Value::string("", call.head));
                                    record.push("message", Value::string(
                                        "No active page. Open a URL first with 'browse open <url>'.",
                                        call.head,
                                    ));
                                } else {
                                    let eval_result = page_eval_only(&chaser, &js, real_eval).await;
                                    match eval_result {
                                        Ok(result) => {
                                            record.push("status", Value::string("success", call.head));
                                            record.push("url", Value::string(current_url, call.head));
                                            record.push("eval", Value::string(result, call.head));
                                        }
                                        Err(e) => {
                                            let err_msg = e.to_string();
                                            record.push("status", Value::string("error", call.head));
                                            record.push("url", Value::string(current_url, call.head));
                                            record.push("message", Value::string(err_msg, call.head));
                                        }
                                    }
                                }
                            }
                            None => {
                                record.push("status", Value::string("error", call.head));
                                record.push("url", Value::string("", call.head));
                                record.push("message", Value::string(
                                    "No active page. Open a URL first with 'browse open <url>'.",
                                    call.head,
                                ));
                            }
                        }

                        std::mem::forget(browser);
                    }
                    (None, None) => {
                        if has_active_session(&cwd) {
                            record.push("status", Value::string("opened", call.head));
                            record.push("url", Value::string("", call.head));
                            record.push("profile", Value::string(&profile_path, call.head));
                        } else {
                            let (browser, _page) = launch_persistent(&cwd).await.map_err(|e| {
                                LabeledError::new(format!("{e}")).with_label("browse open failed", call.head)
                            })?;
                            record.push("status", Value::string("opened", call.head));
                            record.push("url", Value::string("", call.head));
                            record.push("profile", Value::string(&profile_path, call.head));
                            std::mem::forget(browser);
                        }
                    }
                }

                Ok(Value::record(record, call.head))
            })
        } else {
            // ---- Ephemeral browser logic (original http browse behavior) ----
            rt.block_on(async move {
                let url = match url {
                    Some(u) => u,
                    None => {
                        let mut record = Record::new();
                        record.push("status", Value::string("error", call.head));
                        record.push("url", Value::string("", call.head));
                        record.push(
                            "message",
                            Value::string("url is required for ephemeral browse", call.head),
                        );
                        return Ok(Value::record(record, call.head));
                    }
                };

                if let Err(msg) = ensure_url(&url) {
                    let mut record = Record::new();
                    record.push("status", Value::string("error", call.head));
                    record.push("url", Value::string(&url, call.head));
                    record.push("message", Value::string(msg, call.head));
                    return Ok(Value::record(record, call.head));
                }

                if has_active_session(&cwd) {
                    let mut record = Record::new();
                    record.push("status", Value::string("error", call.head));
                    record.push("url", Value::string(&url, call.head));
                    record.push("message", Value::string(
                        "Persistent browser is active. Use 'browse open' or 'browse close' first.",
                        call.head,
                    ));
                    return Ok(Value::record(record, call.head));
                }

                let result: Result<(Browser, Page), Box<dyn Error>> =
                    async { launch_ephemeral(with_head).await }.await;

                let (mut browser, page) = result.map_err(|e| {
                    LabeledError::new(format!("{e}")).with_label("browse failed", call.head)
                })?;

                let chaser = ChaserPage::new(page);

                let nav_result = page_navigate(
                    &chaser,
                    &url,
                    stealth,
                    wait,
                    init_script.as_deref(),
                    eval_js.as_deref(),
                    real_eval,
                    ntrace_opt,
                    call.head,
                )
                .await;

                let (content, network_records, init_errors) = match nav_result {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = browser.close().await;
                        let err_msg = e.to_string();
                        let mut record = Record::new();
                        if err_msg.starts_with("eval error: ") {
                            record.push("status", Value::string("error", call.head));
                            record.push("url", Value::string(&url, call.head));
                            record.push("message", Value::string(err_msg, call.head));
                            return Ok(Value::record(record, call.head));
                        }
                        return Err(
                            LabeledError::new(err_msg).with_label("browse failed", call.head)
                        );
                    }
                };

                let _ = browser.close().await;

                let mut record = Record::new();
                record.push("status", Value::string("success", call.head));
                record.push("url", Value::string(&url, call.head));
                if eval_js.is_some() {
                    record.push(
                        "eval",
                        Value::string(content.unwrap_or_default(), call.head),
                    );
                } else {
                    record.push(
                        "content",
                        Value::string(content.unwrap_or_default(), call.head),
                    );
                }
                if let Some(net) = network_records {
                    record.push(
                        "network",
                        Value::list(
                            net.into_iter()
                                .map(|r| Value::record(r, call.head))
                                .collect(),
                            call.head,
                        ),
                    );
                }
                if !init_errors.is_empty() {
                    record.push(
                        "init_errors",
                        Value::list(
                            init_errors
                                .into_iter()
                                .map(|e| Value::string(e, call.head))
                                .collect(),
                            call.head,
                        ),
                    );
                }

                Ok(Value::record(record, call.head))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Command: browse open
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BrowseOpen;

impl SimplePluginCommand for BrowseOpen {
    type Plugin = BrowsePlugin;

    fn name(&self) -> &str {
        "browse open"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse open")
            .optional("url", SyntaxShape::String, "URL to navigate to (optional)")
            .switch("no-stealth", "Disable stealth mode", None)
            .named(
                "init-script",
                SyntaxShape::Filepath,
                "JS file to inject before page scripts",
                None,
            )
            .named(
                "eval",
                SyntaxShape::String,
                "JS to execute after page load in isolated world (or on current page if no url)",
                Some('e'),
            )
            .named(
                "real-eval",
                SyntaxShape::String,
                "JS to execute after page load in the main world (or on current page if no url)",
                None,
            )
            .named(
                "wait",
                SyntaxShape::Duration,
                "Time to wait before retrieving html",
                Some('w'),
            )
            .named(
                "ntrace",
                SyntaxShape::String,
                "Trace network requests/responses (request, response, or regex pattern)",
                None,
            )
            .category(Category::Network)
    }

    fn description(&self) -> &str {
        "Open a persistent browser or operate on the current page."
    }

    fn extra_description(&self) -> &str {
        "Opens a browser window that stays alive across nushell calls. \
         Always keeps exactly 1 page -- re-opening a URL closes the previous one. \
         Without a URL but with --eval, executes JS on the current page. \
         Call 'browse close' when done. Requires chrome/chromium installed."
    }

    fn examples(&'_ self) -> Vec<Example<'_>> {
        vec![
            Example {
                description: "Open a browser and navigate",
                example: "browse open https://example.com/login",
                result: None,
            },
            Example {
                description: "Eval on the current page",
                example: "browse open --eval \"document.title\"",
                result: None,
            },
            Example {
                description: "Eval in the main world",
                example: "browse open --real-eval \"window.location.href\"",
                result: None,
            },
            Example {
                description: "Navigate to a new page (closes the old one)",
                example: "browse open https://example.com/dashboard",
                result: None,
            },
        ]
    }

    fn run(
        &self,
        _plugin: &BrowsePlugin,
        _engine: &EngineInterface,
        call: &EvaluatedCall,
        input: &Value,
    ) -> Result<Value, LabeledError> {
        let url: Option<String> = call.opt(0)?;
        let stealth = !call.has_flag("no-stealth")?;
        let wait = call.get_flag::<Duration>("wait")?;
        let init_script: Option<String> = call.get_flag("init-script")?;

        let eval_info = resolve_eval_js_and_mode(call, input);
        let (eval_js, real_eval) = match eval_info {
            Some((js, re)) => (Some(js), re),
            None => (None, false),
        };
        let ntrace_str: Option<String> = call.get_flag("ntrace")?;
        let ntrace_opt = ntrace_str.as_deref().map(parse_ntrace);

        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            LabeledError::new(format!("{e}")).with_label("browse open failed", call.head)
        })?;

        rt.block_on(async move {
            let session_path = session_file(&cwd).to_string_lossy().into_owned();
            let profile_path = profile_dir(&cwd).to_string_lossy().into_owned();

            let mut record = Record::new();
            record.push("session", Value::string(&session_path, call.head));
            record.push("port", Value::int(DEFAULT_DEBUG_PORT as i64, call.head));

            match (url, eval_js) {
                (Some(url), eval_js) => {
                    if let Err(msg) = ensure_url(&url) {
                        record.push("status", Value::string("error", call.head));
                        record.push("url", Value::string(&url, call.head));
                        record.push("message", Value::string(msg, call.head));
                        return Ok(Value::record(record, call.head));
                    }
                    // Destroy old session if exists, start fresh
                    try_close_existing(&cwd).await;

                    let (browser, page) = launch_persistent(&cwd).await.map_err(|e| {
                        LabeledError::new(format!("{e}"))
                            .with_label("browse open failed", call.head)
                    })?;

                    let chaser = ChaserPage::new(page);
                    let nav_result = page_navigate(
                        &chaser,
                        &url,
                        stealth,
                        wait,
                        init_script.as_deref(),
                        eval_js.as_deref(),
                        real_eval,
                        ntrace_opt,
                        call.head,
                    )
                    .await;

                    let (content, network_records, init_errors) = match nav_result {
                        Ok(r) => r,
                        Err(e) => {
                            let err_msg = e.to_string();
                            if err_msg.starts_with("eval error: ") {
                                record.push("status", Value::string("error", call.head));
                                record.push("url", Value::string(&url, call.head));
                                record.push("message", Value::string(err_msg, call.head));
                                return Ok(Value::record(record, call.head));
                            }
                            return Err(LabeledError::new(err_msg)
                                .with_label("browse open failed", call.head));
                        }
                    };


                    record.push("status", Value::string("opened", call.head));
                    record.push("url", Value::string(&url, call.head));
                    record.push("profile", Value::string(&profile_path, call.head));
                    if eval_js.is_some() {
                        record.push(
                            "eval",
                            Value::string(content.unwrap_or_default(), call.head),
                        );
                    }
                    if let Some(net) = network_records {
                        record.push(
                            "network",
                            Value::list(
                                net.into_iter()
                                    .map(|r| Value::record(r, call.head))
                                    .collect(),
                                call.head,
                            ),
                        );
                    }
                    if !init_errors.is_empty() {
                        record.push(
                            "init_errors",
                            Value::list(
                                init_errors
                                    .into_iter()
                                    .map(|e| Value::string(e, call.head))
                                    .collect(),
                                call.head,
                            ),
                        );
                    }

                    std::mem::forget(browser);
                }
                (None, Some(js)) => {
                    // Eval on current page — requires existing session
                    if !has_active_session(&cwd) {
                        record.push("status", Value::string("error", call.head));
                        record.push("url", Value::string("", call.head));
                        record.push(
                            "message",
                            Value::string(
                                "No active browser. Open a URL first with 'browse open <url>'.",
                                call.head,
                            ),
                        );
                        return Ok(Value::record(record, call.head));
                    }

                    let ws_url = load_ws_url(&cwd).ok_or_else(|| {
                        LabeledError::new("no session url")
                            .with_label("browse open failed", call.head)
                    })?;
                    let (mut browser, mut handler) =
                        Browser::connect(&ws_url).await.map_err(|e| {
                            LabeledError::new(format!("{e}"))
                                .with_label("browse open failed", call.head)
                        })?;
                    let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

                    let _ = browser.fetch_targets().await;
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    let page = {
                        let all_pages = browser.pages().await.unwrap_or_default();
                        let mut found = None;
                        for p in &all_pages {
                            if let Ok(Some(purl)) = p.url().await
                                && !purl.starts_with("chrome://")
                                && !purl.starts_with("devtools://")
                                && !purl.starts_with("about:blank")
                            {
                                found = Some(p.clone());
                                break;
                            }
                        }
                        found
                    };

                    match page {
                        Some(page) => {
                            let chaser = ChaserPage::new(page);
                            let current_url =
                                chaser.url().await.unwrap_or_default().unwrap_or_default();
                            if current_url.is_empty() || current_url == "about:blank" {
                                record.push("status", Value::string("error", call.head));
                                record.push("url", Value::string("", call.head));
                                record.push("message", Value::string(
                                    "No active page. Open a URL first with 'browse open <url>'.",
                                    call.head,
                                ));
                            } else {
                                let eval_result = page_eval_only(&chaser, &js, real_eval).await;
                                match eval_result {
                                    Ok(result) => {
                                        record.push("status", Value::string("success", call.head));
                                        record.push("url", Value::string(current_url, call.head));
                                        record.push("eval", Value::string(result, call.head));
                                    }
                                    Err(e) => {
                                        let err_msg = e.to_string();
                                        record.push("status", Value::string("error", call.head));
                                        record.push("url", Value::string(current_url, call.head));
                                        record.push("message", Value::string(err_msg, call.head));
                                    }
                                }
                            }
                        }
                        None => {
                            record.push("status", Value::string("error", call.head));
                            record.push("url", Value::string("", call.head));
                            record.push(
                                "message",
                                Value::string(
                                    "No active page. Open a URL first with 'browse open <url>'.",
                                    call.head,
                                ),
                            );
                        }
                    }

                    std::mem::forget(browser);
                }
                (None, None) => {
                    if has_active_session(&cwd) {
                        record.push("status", Value::string("opened", call.head));
                        record.push("url", Value::string("", call.head));
                        record.push("profile", Value::string(&profile_path, call.head));
                    } else {
                        let (browser, _page) = launch_persistent(&cwd).await.map_err(|e| {
                            LabeledError::new(format!("{e}"))
                                .with_label("browse open failed", call.head)
                        })?;
                        record.push("status", Value::string("opened", call.head));
                        record.push("url", Value::string("", call.head));
                        record.push("profile", Value::string(&profile_path, call.head));
                        std::mem::forget(browser);
                    }
                }
            }

            Ok(Value::record(record, call.head))
        })
    }
}

// ---------------------------------------------------------------------------
// Command: browse close
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BrowseClose;

impl SimplePluginCommand for BrowseClose {
    type Plugin = BrowsePlugin;

    fn name(&self) -> &str {
        "browse close"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse close").category(Category::Network)
    }

    fn description(&self) -> &str {
        "Close the persistent browser opened by 'browse open'."
    }

    fn extra_description(&self) -> &str {
        "Closes the browser and removes the session file. \
         The profile directory (.nu_browse_profile) is preserved for next time."
    }

    fn examples(&'_ self) -> Vec<Example<'_>> {
        vec![Example {
            description: "Close the persistent browser",
            example: "browse close",
            result: None,
        }]
    }

    fn run(
        &self,
        _plugin: &BrowsePlugin,
        _engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: &Value,
    ) -> Result<Value, LabeledError> {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            LabeledError::new(format!("{e}")).with_label("browse close failed", call.head)
        })?;

        rt.block_on(async {
            if !has_active_session(&cwd) {
                let mut record = Record::new();
                record.push("status", Value::string("no_session", call.head));
                return Ok(Value::record(record, call.head));
            }

            try_close_existing(&cwd).await;

            let mut record = Record::new();
            record.push("status", Value::string("closed", call.head));
            Ok(Value::record(record, call.head))
        })
    }
}
