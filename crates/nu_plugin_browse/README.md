# nu_plugin_browse

Nushell 的 headless 浏览器插件，基于 [chaser-oxide](https://github.com/nickolay/chaser-oxide)（CDP 协议），需要系统安装 Chrome 或 Chromium。

## 安装

```nu
cargo build -p nu_plugin_browse
plugin add target/debug/nu_plugin_browse
plugin use browse
```

## 命令

### `browse <url>`

一次性获取页面内容。启动临时浏览器，请求完成后自动关闭。

```nu
# 获取页面 HTML
browse https://example.com

# 禁用隐身模式
browse https://example.com --no-stealth

# 显示浏览器窗口（调试用）
browse https://example.com --with-head

# 等待额外时间后获取内容
browse https://example.com --wait 2sec

# 注入初始化脚本后获取
browse https://example.com --init-script ./hook.js
```

返回值：

| 字段 | 类型 | 说明 |
|------|------|------|
| `status` | string | `"success"` 或 `"error"` |
| `url` | string | 请求的 URL |
| `content` | string | 页面 HTML（未指定 `--eval` 时返回） |
| `eval` | string | JS 执行结果（指定 `--eval` 或 `--real-eval` 时返回） |
| `message` | string | 错误信息（`status == "error"` 时返回） |
| `network` | list\<record\> | 网络追踪记录（指定 `--ntrace` 时返回） |
| `init_errors` | list\<string\> | 初始化脚本错误（指定 `--init-script` 且脚本有错时返回） |

### `browse <url> --open`

等价于 `browse open <url>`，打开持久浏览器并导航到指定 URL。

### `browse open [url]`

打开或连接持久浏览器。浏览器窗口跨调用复用，保留 cookie/localStorage 等状态。

```nu
# 打开并导航
browse open https://example.com

# 连接已有浏览器（不传 URL）
browse open

# 在当前页面执行 JS
browse open --eval "document.title"
browse open --real-eval "window.location.href"

# 导航到新页面（自动关闭旧页面）
browse open https://example.com/other
```

返回值：

| 字段 | 类型 | 说明 |
|------|------|------|
| `status` | string | `"opened"` / `"success"` / `"error"` |
| `url` | string | 当前页面 URL |
| `session` | string | session 文件路径 |
| `port` | int | CDP 调试端口（默认 9223） |
| `profile` | string | Chrome profile 目录路径 |
| `eval` | string | JS 执行结果（指定 `--eval`/`--real-eval` 时返回） |
| `message` | string | 错误信息（`status == "error"` 时返回） |
| `network` | list\<record\> | 网络追踪记录（指定 `--ntrace` 时返回） |
| `init_errors` | list\<string\> | 初始化脚本错误（指定 `--init-script` 且脚本有错时返回） |

### `browse close`

关闭持久浏览器并清理 session 文件。profile 目录保留供下次使用。

```nu
browse close
```

返回值：

| 字段 | 类型 | 说明 |
|------|------|------|
| `status` | string | `"closed"` 或 `"no_session"` |

## 参数

### `--eval <js>` / `-e <js>`

在隔离世界（isolated world）中执行 JavaScript。不污染页面全局变量，无法访问页面定义的 JS 变量。支持管道输入。

```nu
# 直接传参
browse https://example.com --eval "document.title"

# 管道输入
"document.title" | browse https://example.com --eval $in

# 返回值自动 JSON.stringify
browse https://example.com --eval "[1, 2, 3]"
# => { status: "success", eval: "[1,2,3]" }

# JS 错误不会抛异常，返回 error record
browse https://example.com --eval "undefinedVar.test"
# => { status: "error", message: "eval error: ReferenceError: ..." }
```

### `--real-eval <js>`

在主世界（main world）中执行 JavaScript。可以访问和修改页面全局变量、React/Vue 等框架的内部状态。与 `--eval` 互斥，`--real-eval` 优先。

```nu
# 访问页面 JS 变量
browse https://example.com --real-eval "window.appState"

# 修改页面状态
browse https://example.com --real-eval "window.darkMode = true"
```

### `--init-script <path>`

在页面脚本执行前注入 JavaScript 文件。适用于拦截网络请求、修改全局对象、注入 mock 数据等场景。仅在注入脚本有错误时启用 `Runtime.enable`，其余情况关闭以避免被反检测系统发现。

```nu
# 注入脚本并检测错误
browse https://example.com --init-script ./hook.js --real-eval "window.__INJECTED_VAR"
```

初始化脚本的运行时错误和语法错误会被捕获到 `init_errors` 字段，格式为 `行号:列号: 错误信息`。

### `--ntrace <pattern>`

网络追踪，记录请求和响应的完整信息（含 headers 和 response body）。

Pattern 格式：

| pattern | 说明 |
|---------|------|
| `".*"` | 匹配所有请求和响应 |
| `"request"` | 仅请求 |
| `"response"` | 仅响应 |
| `"request:regex"` | 匹配 URL 的请求 |
| `"response:regex"` | 匹配 URL 的响应 |
| `"regex"` | 匹配 URL 的请求和响应 |

```nu
# 追踪所有网络请求
browse https://example.com --ntrace '.*'

# 仅追踪 JS 文件响应
browse https://example.com --ntrace 'response:\.js'

# 仅追踪特定域名的请求
browse https://example.com --ntrace 'request:api\.example\.com'
```

`network` 字段中每条 record 的结构：

**request 类型：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `type` | string | `"request"` |
| `method` | string | HTTP 方法 |
| `url` | string | 请求 URL |
| `headers` | string | 完整请求头 |

**response 类型：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `type` | string | `"response"` |
| `id` | string | CDP request ID |
| `status` | int | HTTP 状态码 |
| `url` | string | 响应 URL |
| `mime` | string | MIME 类型 |
| `headers` | string | 完整响应头 |
| `body` | string | 响应体（部分资源类型可能获取失败） |

### `--no-stealth`

默认启用隐身模式（stealth），通过修改 `navigator.webdriver` 等属性来规避反爬检测。`--no-stealth` 关闭此功能。

### `--with-head`

显示浏览器窗口（仅适用于 ephemeral 模式）。持久浏览器（`browse open`）默认显示窗口。

### `--wait <duration>` / `-w <duration>`

页面加载后额外等待指定时间再获取内容。插件还会自动等待网络空闲（XHR/fetch 请求全部完成）。

## Session 管理

- 持久浏览器的 profile 存储在当前工作目录下的 `.nu_browse_profile/`
- session 信息存储在 `.nu_browse_profile/.session`
- 单 session 覆盖模式：`browse open <url>` 会自动回收旧 session
- `browse close` 关闭浏览器并删除 session 文件，profile 目录保留
- ephemeral 模式在持久浏览器活跃时会被拒绝

## 错误处理

所有命令的错误均通过 record 返回，不会抛出 Nushell 异常：

- JS 执行错误：`{ status: "error", message: "eval error: ..." }`
- 缺少 URL：`{ status: "error", message: "url is required for ephemeral browse" }`
- 持久浏览器冲突：`{ status: "error", message: "Persistent browser is active..." }`
- 初始化脚本错误：通过 `init_errors` 字段返回，不影响主流程
