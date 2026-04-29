use nu_plugin::{EngineInterface, EvaluatedCall, SimplePluginCommand};
use nu_protocol::{Category, Example, LabeledError, Record, Signature, SyntaxShape, Type, Value};
use std::error::Error;
use std::time::Duration;

use crate::commands::browse_open::{PersistentParams, run_persistent};
use crate::launch::launch_ephemeral;
use crate::page::{NavigateParams, page_navigate};
use crate::session::has_active_session;
use crate::utils::{ensure_url, parse_ntrace, resolve_eval_js_and_mode};

use chaser_oxide::ChaserPage;

#[derive(Clone)]
pub struct Browse;

impl SimplePluginCommand for Browse {
    type Plugin = crate::BrowsePlugin;

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
            .input_output_types(vec![
                (Type::Nothing, Type::record()),
                (Type::String, Type::record()),
            ])
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

    fn search_terms(&self) -> Vec<&str> {
        vec![
            "browse", "web", "scrape", "headless", "chrome", "chromium", "browser",
        ]
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
        _plugin: &crate::BrowsePlugin,
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
            // Delegate to shared persistent browser logic
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
        } else {
            // Ephemeral browser logic (unique to Browse)
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

                let result: Result<(chaser_oxide::Browser, chaser_oxide::Page), Box<dyn Error>> =
                    launch_ephemeral(with_head, &cwd).await;

                let (mut browser, page) = result.map_err(|e| {
                    LabeledError::new(format!("{e}")).with_label("browse failed", call.head)
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
                        span: call.head,
                    },
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
