#!/usr/bin/env nu
# browse 插件功能测试脚本 — 目标: www.baidu.com

use std/assert

const TARGET = "https://www.baidu.com"

# 清理：关闭残留浏览器
try { browse close } catch { }

# ─────────────────────────────────────────────────────────────────
print $"(char nl)=== 1. 一次性浏览 ephemeral ==="
sleep 500ms
let r1 = browse $TARGET
print $"  status: ($r1.status)"
print $"  url: ($r1.url)"
print $"  content length: ($r1.content | str length) bytes"
assert ($r1.status == "success")
assert ($r1.url == $TARGET)
assert (($r1.content | str length) > 1000)

# ─────────────────────────────────────────────────────────────────
print $"\n=== 2. --eval 隔离世界 JS ==="
sleep 500ms
let r2 = browse $TARGET --eval "document.title"
print $"  status: ($r2.status)"
print $"  eval: ($r2.eval)"
assert ($r2.status == "success")
# JSON.stringify 会加引号，trim 掉
assert (($r2.eval | str replace -a '"' "") =~ "百度")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 3. --eval 管道输入 ==="
let r3 = "document.title" | browse $TARGET --eval $in
print $"  status: ($r3.status)"
print $"  eval: ($r3.eval)"
assert ($r3.status == "success")
assert (($r3.eval | str replace -a '"' "") =~ "百度")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 4. --real-eval 主世界 JS ==="
sleep 500ms
let r4 = browse $TARGET --real-eval "window.__NEXT_DATA__ !== undefined ? 'exists' : 'not_found'"
print $"  status: ($r4.status)"
print $"  eval: ($r4.eval)"
assert ($r4.status == "success")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 5. --real-eval 访问页面 DOM ==="
sleep 500ms
let r5 = browse $TARGET --real-eval "document.querySelector('#su').value"
print $"  status: ($r5.status)"
print $"  eval placeholder: ($r5.eval)"
assert ($r5.status == "success")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 6. --real-eval 与 --eval 互斥（--real-eval 优先）==="
sleep 500ms
let r6 = browse $TARGET --real-eval "1+1" --eval "2+2"
print $"  status: ($r6.status)"
print $"  eval: ($r6.eval)"
assert ($r6.status == "success")
# JSON.stringify(2) returns "2" as string, which gets JSON-stringified again
let eval_val = ($r6.eval | str trim -c '"')
assert ($eval_val == "2")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 7. JS 错误捕获 ==="
sleep 500ms
let r7 = browse $TARGET --eval "undefinedVar.test"
print $"  status: ($r7.status)"
assert ($r7.status == "error")
assert ($r7.message =~ "eval error")
assert ($r7.message =~ "ReferenceError")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 8. JS 正常返回各种类型 ==="
sleep 500ms
let r8a = browse $TARGET --eval "42"
assert ($r8a.status == "success")
assert ($r8a.eval == "42")

sleep 500ms
let r8b = browse $TARGET --eval "'hello'"
assert ($r8b.status == "success")
assert ($r8b.eval =~ "hello")

sleep 500ms
let r8c = browse $TARGET --eval "[1, 2, 3]"
assert ($r8c.status == "success")
assert ($r8c.eval == "[1,2,3]")

sleep 500ms
let r8d = browse $TARGET --eval "null"
assert ($r8d.status == "success")
assert ($r8d.eval == "null")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 9. --ntrace 网络追踪 ==="
sleep 500ms
# 用 _t 参数避免浏览器缓存
sleep 500ms
let r9 = browse $"($TARGET)?_t=(date now | format date '%s')" --ntrace '.*'
print $"  status: ($r9.status)"
assert ($r9.status == "success")
assert ($r9.network? | is-not-empty)
let reqs = $r9.network | where type == request
let ress = $r9.network | where type == response
print $"  total entries: ($r9.network | length)"
print $"  requests: ($reqs | length)"
print $"  responses: ($ress | length)"
assert (($reqs | length) > 0)
assert (($ress | length) > 0)

# request 有 headers
let first_req = $reqs | first
assert ($first_req.headers? != null)
print $"  sample request headers: ($first_req.headers | str substring 0..<80)..."

# response 有 headers + body
let first_res = $ress | first
assert ($first_res.headers? != null)
assert ($first_res.mime? != null)
print $"  sample response: ($first_res.url) status=($first_res.status) mime=($first_res.mime)"

# response body 非空
if ($first_res.body? != null) {
    print $"  sample body length: ($first_res.body | str length) bytes"
}
let bodies = ($ress | get body? | compact)
print $"  responses with body: ($bodies | length) / ($ress | length)"
# main document 的 body 应该包含 html
let main_doc = $ress | where mime =~ 'html'
assert (($main_doc | length) >= 1)
let main_body = ($main_doc | first).body
assert ($main_body =~ "<html")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 10. --ntrace 按 MIME 过滤 ==="
let js_res = $r9.network | where type == response | where mime =~ 'javascript'
print $"  JS responses: ($js_res | length)"
if ($js_res | length) > 0 {
    $js_res | select url status mime | take 5 | table -e
} else {
    print "  (no JS resources in this fetch)"
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 11. --ntrace 按 MIME 过滤图片 ==="
let img_res = $r9.network | where type == response | where mime =~ 'image'
print $"  image responses: ($img_res | length)"
if ($img_res | length) > 0 {
    $img_res | select url status mime | take 3 | table -e
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 12. --ntrace 仅 request ==="
sleep 500ms
let r12 = browse $TARGET --ntrace 'request'
assert ($r12.status == "success")
if ($r12.network? | is-not-empty) {
    let types = $r12.network | get type | uniq
    print $"  types: ($types)"
    assert ($types == ["request"])
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 13. --ntrace 仅 response ==="
sleep 500ms
let r13 = browse $TARGET --ntrace 'response'
assert ($r13.status == "success")
if ($r13.network? | is-not-empty) {
    let types = $r13.network | get type | uniq
    print $"  types: ($types)"
    assert ($types == ["response"])
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 14. --ntrace 无匹配 ==="
sleep 500ms
let r14 = browse $TARGET --ntrace 'nonexistent_pattern_xyz'
assert ($r14.status == "success")
if ($r14.network? | is-not-empty) {
    print $"  entries: ($r14.network | length)"
} else {
    print "  entries: 0 (empty)"
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 15. --ntrace request URL 正则 ==="
sleep 500ms
let r15 = browse $TARGET --ntrace 'request:baidu\.com'
assert ($r15.status == "success")
if ($r15.network? | is-not-empty) {
    let urls = $r15.network | get url
    print $"  matched URLs: ($urls | length)"
    assert ($urls | all { $in =~ 'baidu\.com' })
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 16. --ntrace response URL 正则 ==="
sleep 500ms
let r16 = browse $TARGET --ntrace 'response:\.js'
assert ($r16.status == "success")
if ($r16.network? | is-not-empty) {
    let urls = $r16.network | get url
    print $"  matched URLs: ($urls | length)"
    assert ($urls | all { $in =~ '\.js' })
}

# ─────────────────────────────────────────────────────────────────
print $"\n=== 17. --open 持久浏览器 ==="
let r17 = browse $TARGET --open
print $"  status: ($r17.status)"
print $"  url: ($r17.url)"
print $"  profile: ($r17.profile)"
print $"  session: ($r17.session)"
assert ($r17.status == "opened")
assert ($r17.url == $TARGET)
assert ($r17.session | is-not-empty)

# ─────────────────────────────────────────────────────────────────
print $"\n=== 18. browse open --eval 持久页面 ==="
let r18 = browse open --eval "document.title"
print $"  status: ($r18.status)"
print $"  eval: ($r18.eval)"
assert ($r18.status == "success")
assert (($r18.eval | str replace -a '"' "") =~ "百度")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 19. browse open --real-eval 主世界 ==="
let r19 = browse open --real-eval "navigator.userAgent"
print $"  status: ($r19.status)"
print $"  UA: ($r19.eval | str substring 0..<80)..."
assert ($r19.status == "success")
assert ($r19.eval =~ "Chrome")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 20. browse open 导航到新页面 ==="
browse open https://www.bilibili.com
let r20 = browse open --eval "document.title"
print $"  status: ($r20.status)"
print $"  title: ($r20.eval)"
assert ($r20.status == "success")
assert ($r20.eval =~ "bilibili")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 21. browse open --real-eval 访问新页面变量 ==="
let r21 = browse open --real-eval "document.querySelector('.bili-header__bar').textContent"
print $"  status: ($r21.status)"
print $"  eval: ($r21.eval)"
assert ($r21.status == "success")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 22. browse open JS 错误 ==="
let r22 = browse open --eval "throw new Error('test error')"
print $"  status: ($r22.status)"
print $"  message: ($r22.message)"
assert ($r22.status == "error")
assert ($r22.message =~ "eval error")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 23. browse --open 等价 browse open ==="
browse close
let r23 = browse $TARGET --open
print $"  status: ($r23.status)"
assert ($r23.status == "opened")
assert ($r23.url == $TARGET)

# ─────────────────────────────────────────────────────────────────
print $"\n=== 24. browse open 无参数连接已有 ==="
let r24 = browse open
print $"  status: ($r24.status)"
assert ($r24.status == "opened")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 25. browse close ==="
let r25 = browse close
print $"  status: ($r25.status)"
assert ($r25.status == "closed")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 26. 无活跃 session 时 browse close ==="
let r26 = browse close
print $"  status: ($r26.status)"
assert ($r26.status == "no_session")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 27. 持久浏览器活跃时 ephemeral 应拒绝 ==="
browse $TARGET --open
sleep 500ms
let r27 = browse $TARGET
print $"  status: ($r27.status)"
assert ($r27.status == "error")
assert ($r27.message =~ "Persistent browser is active")
browse close

# ─────────────────────────────────────────────────────────────────
print $"\n=== 28. --no-stealth ==="
sleep 500ms
let r28 = browse $TARGET --no-stealth
print $"  status: ($r28.status)"
assert ($r28.status == "success")
assert (($r28.content | str length) > 1000)

# ─────────────────────────────────────────────────────────────────
print $"\n=== 29. --wait ==="
sleep 500ms
let r29 = browse $TARGET --wait 1sec
print $"  status: ($r29.status)"
assert ($r29.status == "success")

# ─────────────────────────────────────────────────────────────────
print $"\n=== 30. --init-script 页面脚本前注入 ==="
sleep 500ms
let hook_path = $"($env.TEMP)/nu_browse_test_hook.js"
"window.__NU_BROWSE_INIT_TEST = 'injected';" | save -f $hook_path
let r30 = browse $TARGET --init-script $hook_path --real-eval "window.__NU_BROWSE_INIT_TEST"
print $"  status: ($r30.status)"
print $"  eval: ($r30.eval)"
assert ($r30.status == "success")
assert ($r30.eval =~ "injected")
rm -f $hook_path

# ─────────────────────────────────────────────────────────────────
print $"\n=== 31. --init-script 运行时错误捕获 ==="
sleep 500ms
let bad_hook = $"($env.TEMP)/nu_browse_test_runtime_err.js"
"undefinedVar.test;" | save -f $bad_hook
sleep 500ms
let r31 = browse $TARGET --init-script $bad_hook --real-eval "1+1"
print $"  status: ($r31.status)"
print $"  init_errors: ($r31.init_errors?)"
assert ($r31.status == "success")
assert ($r31.init_errors? | is-not-empty)
assert (($r31.init_errors | first) =~ "ReferenceError")
rm -f $bad_hook

# ─────────────────────────────────────────────────────────────────
print $"\n=== 32. --init-script 语法错误捕获 ==="
sleep 500ms
let bad_hook2 = $"($env.TEMP)/nu_browse_test_syntax_err.js"
"function( {" | save -f $bad_hook2
sleep 500ms
let r32 = browse $TARGET --init-script $bad_hook2 --real-eval "1+1"
print $"  status: ($r32.status)"
print $"  init_errors: ($r32.init_errors?)"
assert ($r32.status == "success")
assert ($r32.init_errors? | is-not-empty)
assert (($r32.init_errors | first) =~ "SyntaxError")
rm -f $bad_hook2

# ─────────────────────────────────────────────────────────────────
print $"\n=== 33. ephemeral 无 URL 应报错 ==="
sleep 500ms
let r30 = browse
print $"  status: ($r30.status)"
assert ($r30.status == "error")

# ─────────────────────────────────────────────────────────────────
# 清理
try { browse close } catch { }

print $"\n=== 所有 33 项测试通过 ==="
