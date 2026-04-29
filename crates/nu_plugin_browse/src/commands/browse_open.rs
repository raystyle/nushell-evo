use chaser_oxide::{Browser, ChaserPage};
use futures::StreamExt;
use nu_plugin::{EngineInterface, EvaluatedCall, SimplePluginCommand};
use nu_protocol::{
    Category, Example, LabeledError, Record, Signature, Span, SyntaxShape, Type, Value,
};
use regex::Regex;
use std::mem::ManuallyDrop;
use std::time::Duration;

use crate::launch::launch_persistent;
use crate::page::{NavigateParams, page_eval_only, page_navigate};
use crate::session::{
    DEFAULT_DEBUG_PORT, has_active_session, load_ws_url, profile_dir, session_file,
    try_close_existing,
};
use crate::utils::{ensure_url, parse_ntrace, resolve_eval_js_and_mode};

/// Parameters collected from command flags, shared between Browse and BrowseOpen.
pub struct PersistentParams {
    pub url: Option<String>,
    pub stealth: bool,
    pub with_head: bool,
    pub wait: Option<Duration>,
    pub init_script: Option<String>,
    pub eval_js: Option<String>,
    pub real_eval: bool,
    pub ntrace_opt: Option<(bool, bool, Option<Regex>)>,
    pub cwd: String,
    pub span: Span,
}

/// Shared persistent browser logic used by both `browse open` and `browse --open`.
pub async fn run_persistent(params: PersistentParams) -> Result<Value, LabeledError> {
    let PersistentParams {
        url,
        stealth,
        with_head,
        wait,
        init_script,
        eval_js,
        real_eval,
        ntrace_opt,
        cwd,
        span,
    } = params;

    let session_path = session_file(&cwd).to_string_lossy().into_owned();
    let profile_path = profile_dir(&cwd).to_string_lossy().into_owned();

    let mut record = Record::new();
    record.push("session", Value::string(&session_path, span));
    record.push("port", Value::int(DEFAULT_DEBUG_PORT as i64, span));

    match (url, eval_js) {
        (Some(url), eval_js) => {
            if let Err(msg) = ensure_url(&url) {
                record.push("status", Value::string("error", span));
                record.push("url", Value::string(&url, span));
                record.push("message", Value::string(msg, span));
                return Ok(Value::record(record, span));
            }
            try_close_existing(&cwd).await;

            let (browser, page) = launch_persistent(with_head, &cwd).await.map_err(|e| {
                LabeledError::new(format!("{e}")).with_label("browse open failed", span)
            })?;

            let chaser = ChaserPage::new(page);
            let nav_result = page_navigate(
                &chaser,
                &NavigateParams {
                    url: &url,
                    stealth,
                    wait,
                    init_script: init_script.as_deref(),
                    eval_js: eval_js.as_deref(),
                    real_eval,
                    ntrace: ntrace_opt,
                    span,
                },
            )
            .await;

            let (content, network_records, init_errors) = match nav_result {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.starts_with("eval error: ") {
                        record.push("status", Value::string("error", span));
                        record.push("url", Value::string(&url, span));
                        record.push("message", Value::string(err_msg, span));
                        return Ok(Value::record(record, span));
                    }
                    return Err(LabeledError::new(err_msg).with_label("browse open failed", span));
                }
            };

            record.push("status", Value::string("opened", span));
            record.push("url", Value::string(&url, span));
            record.push("profile", Value::string(&profile_path, span));
            if eval_js.is_some() {
                record.push("eval", Value::string(content.unwrap_or_default(), span));
            }
            if let Some(net) = network_records {
                record.push(
                    "network",
                    Value::list(
                        net.into_iter().map(|r| Value::record(r, span)).collect(),
                        span,
                    ),
                );
            }
            if !init_errors.is_empty() {
                record.push(
                    "init_errors",
                    Value::list(
                        init_errors
                            .into_iter()
                            .map(|e| Value::string(e, span))
                            .collect(),
                        span,
                    ),
                );
            }

            let _browser = ManuallyDrop::new(browser);
        }
        (None, Some(js)) => {
            if !has_active_session(&cwd) {
                record.push("status", Value::string("error", span));
                record.push("url", Value::string("", span));
                record.push(
                    "message",
                    Value::string(
                        "No active browser. Open a URL first with 'browse open <url>'.",
                        span,
                    ),
                );
                return Ok(Value::record(record, span));
            }

            let ws_url = load_ws_url(&cwd).ok_or_else(|| {
                LabeledError::new("no session url").with_label("browse open failed", span)
            })?;
            let (mut browser, mut handler) = Browser::connect(&ws_url).await.map_err(|e| {
                LabeledError::new(format!("{e}")).with_label("browse open failed", span)
            })?;
            let _h = tokio::spawn(async move { while handler.next().await.is_some() {} });

            let _ = browser.fetch_targets().await;
            tokio::time::sleep(Duration::from_millis(100)).await; // allow targets to register

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
                        record.push("status", Value::string("error", span));
                        record.push("url", Value::string("", span));
                        record.push(
                            "message",
                            Value::string(
                                "No active page. Open a URL first with 'browse open <url>'.",
                                span,
                            ),
                        );
                    } else {
                        let eval_result = page_eval_only(&chaser, &js, real_eval).await;
                        match eval_result {
                            Ok(result) => {
                                record.push("status", Value::string("success", span));
                                record.push("url", Value::string(current_url, span));
                                record.push("eval", Value::string(result, span));
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                record.push("status", Value::string("error", span));
                                record.push("url", Value::string(current_url, span));
                                record.push("message", Value::string(err_msg, span));
                            }
                        }
                    }
                }
                None => {
                    record.push("status", Value::string("error", span));
                    record.push("url", Value::string("", span));
                    record.push(
                        "message",
                        Value::string(
                            "No active page. Open a URL first with 'browse open <url>'.",
                            span,
                        ),
                    );
                }
            }

            let _browser = ManuallyDrop::new(browser);
        }
        (None, None) => {
            if has_active_session(&cwd) {
                record.push("status", Value::string("opened", span));
                record.push("url", Value::string("", span));
                record.push("profile", Value::string(&profile_path, span));
            } else {
                let (browser, _page) = launch_persistent(with_head, &cwd).await.map_err(|e| {
                    LabeledError::new(format!("{e}")).with_label("browse open failed", span)
                })?;
                record.push("status", Value::string("opened", span));
                record.push("url", Value::string("", span));
                record.push("profile", Value::string(&profile_path, span));
                let _browser = ManuallyDrop::new(browser);
            }
        }
    }

    Ok(Value::record(record, span))
}

// ---------------------------------------------------------------------------
// Command: browse open
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BrowseOpen;

impl SimplePluginCommand for BrowseOpen {
    type Plugin = crate::BrowsePlugin;

    fn name(&self) -> &str {
        "browse open"
    }

    fn signature(&self) -> Signature {
        Signature::build("browse open")
            .optional("url", SyntaxShape::String, "URL to navigate to (optional)")
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
            .input_output_types(vec![
                (Type::Nothing, Type::record()),
                (Type::String, Type::record()),
            ])
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

    fn search_terms(&self) -> Vec<&str> {
        vec![
            "browse",
            "open",
            "browser",
            "persistent",
            "chrome",
            "chromium",
        ]
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
        _plugin: &crate::BrowsePlugin,
        _engine: &EngineInterface,
        call: &EvaluatedCall,
        input: &Value,
    ) -> Result<Value, LabeledError> {
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
            LabeledError::new(format!("{e}")).with_label("browse open failed", call.head)
        })?;

        rt.block_on(run_persistent(PersistentParams {
            url,
            stealth,
            with_head,
            wait,
            init_script,
            eval_js,
            real_eval,
            ntrace_opt,
            cwd,
            span: call.head,
        }))
    }
}
