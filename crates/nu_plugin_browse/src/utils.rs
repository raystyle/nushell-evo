use nu_plugin::EvaluatedCall;
use nu_protocol::Value;
use regex::Regex;

#[cfg(test)]
use nu_protocol::Span;

pub const EVAL_ERROR_PREFIX: &str = "__NU_EVAL_ERROR__INTERNAL__:";

pub fn resolve_eval_js_and_mode(call: &EvaluatedCall, input: &Value) -> Option<(String, bool)> {
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

pub fn parse_ntrace(value: &str) -> (bool, bool, Option<Regex>) {
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

pub fn wrap_eval_js(js: &str) -> String {
    format!(
        "(function() {{ try {{ var __r = ({}); return __r === undefined ? null : JSON.stringify(__r); }} catch(__e) {{ return '{PREFIX}' + __e.toString(); }} }})()",
        js,
        PREFIX = EVAL_ERROR_PREFIX
    )
}

pub fn check_eval_result(s: &str) -> Result<String, String> {
    if let Some(stripped) = s.strip_prefix(EVAL_ERROR_PREFIX) {
        Err(stripped.to_string())
    } else {
        Ok(s.to_string())
    }
}

pub fn ensure_url(url: &str) -> Result<String, String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err(format!(
            "invalid URL '{url}': must start with http:// or https://"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_url_accepts_http() {
        assert_eq!(
            ensure_url("http://example.com").unwrap(),
            "http://example.com"
        );
    }

    #[test]
    fn test_ensure_url_accepts_https() {
        assert_eq!(
            ensure_url("https://example.com").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn test_ensure_url_rejects_ftp() {
        assert!(ensure_url("ftp://example.com").is_err());
    }

    #[test]
    fn test_ensure_url_rejects_bare() {
        assert!(ensure_url("example.com").is_err());
    }

    #[test]
    fn test_check_eval_result_ok() {
        assert_eq!(check_eval_result("hello").unwrap(), "hello");
    }

    #[test]
    fn test_check_eval_result_error() {
        let result =
            check_eval_result("__NU_EVAL_ERROR__INTERNAL__:TypeError: x is not a function");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "TypeError: x is not a function");
    }

    #[test]
    fn test_check_eval_result_no_false_positive() {
        // A value that starts with the old prefix should NOT match the new one
        let result = check_eval_result("__NU_EVAL_ERROR:something");
        assert!(
            result.is_ok(),
            "old prefix should not trigger false positive"
        );
    }

    #[test]
    fn test_wrap_eval_js_contains_try_catch() {
        let wrapped = wrap_eval_js("document.title");
        assert!(wrapped.contains("try"));
        assert!(wrapped.contains("catch"));
        assert!(wrapped.contains(EVAL_ERROR_PREFIX));
        assert!(wrapped.contains("document.title"));
    }

    #[test]
    fn test_parse_ntrace_request() {
        let (show_req, show_res, regex) = parse_ntrace("request");
        assert!(show_req);
        assert!(!show_res);
        assert!(regex.is_none());
    }

    #[test]
    fn test_parse_ntrace_response() {
        let (show_req, show_res, regex) = parse_ntrace("response");
        assert!(!show_req);
        assert!(show_res);
        assert!(regex.is_none());
    }

    #[test]
    fn test_parse_ntrace_request_with_pattern() {
        let (show_req, show_res, regex) = parse_ntrace("request:https://");
        assert!(show_req);
        assert!(!show_res);
        assert!(regex.is_some());
        assert!(regex.unwrap().is_match("https://example.com"));
    }

    #[test]
    fn test_parse_ntrace_bare_regex() {
        let (show_req, show_res, regex) = parse_ntrace("example\\.com");
        assert!(show_req);
        assert!(show_res);
        assert!(regex.is_some());
        assert!(regex.unwrap().is_match("https://example.com/path"));
    }

    #[test]
    fn test_resolve_eval_nothing_input_returns_none() {
        let call = EvaluatedCall::new(Span::test_data());
        assert!(resolve_eval_js_and_mode(&call, &Value::nothing(Span::test_data())).is_none());
    }
}
