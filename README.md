# nushell-evo

> 基于 [Nushell](https://github.com/nushell/nushell) 的 fork，通过记录 AI 模型使用 Nushell 的命令执行情况，让模型自我进化。

AI 模型（如 Claude）通过 MCP（Model Context Protocol）调用 Nushell 执行命令时，nushell-evo 会自动记录每一次调用的命令、目录、执行结果（成功/失败）和错误详情，生成 JSONL 格式的审计日志。这些日志可以用来：

- **分析** AI 常犯的 Nushell 语法错误模式
- **训练** 让模型更准确地使用 Nushell
- **调试** MCP 会话中的命令执行问题

## 快速开始

### 安装

从 [Release](https://github.com/raystyle/nushell-evo/releases) 下载对应平台的二进制，或从源码编译：

```bash
cargo build --release
```

### 启用日志

`NU_MCP_LOG` 是一个 Nushell 环境变量，支持以下几种配置方式：

#### 方式一：通过 Nushell 配置文件（推荐）

在 `config.nu` 中添加：

```nu
$env.NU_MCP_LOG = "/path/to/nu_evo.jsonl"
```

这样每次启动 MCP server 时日志自动生效。编辑配置文件：

```nu
config nu
```

> `$env.NU_MCP_LOG` 的值是日志文件路径。设置为空字符串时使用默认文件名 `nu_evo.jsonl`。

#### 方式二：在 MCP 会话内动态设置

如果使用 Claude Code 等 MCP 客户端，直接在对话中要求设置即可，例如："在 nushell 中设置 NU_MCP_LOG 为 /tmp/nu_evo.jsonl"。客户端会自动通过 evaluate 工具执行：

```nu
$env.NU_MCP_LOG = "/tmp/nu_evo.jsonl"
```

MCP 会话具有 REPL 语义，环境变量跨调用持久，设置一次后所有后续命令执行都会被记录。

#### 方式三：通过 MCP 客户端的环境变量

某些 MCP 客户端支持在启动配置中传递环境变量。例如 Claude Code 的 MCP 配置（`settings.json`）：

```json
{
  "mcpServers": {
    "nushell": {
      "command": "nu",
      "args": ["--mcp"],
      "env": {
        "NU_MCP_LOG": "/path/to/nu_evo.jsonl"
      }
    }
  }
}
```

> 注意：使用 `--no-config-file` 参数会跳过 config.nu 加载，此时环境变量只能通过客户端 `env` 或 MCP 会话内设置。如果希望从 config.nu 读取配置，不要加 `--no-config-file`。

## 日志格式

每行一条 JSON 记录：

### 成功日志

```json
{
  "timestamp": "2026-04-21T14:08:38+08:00",
  "command": "1 + 1",
  "cwd": "/home/user/project",
  "status": "success"
}
```

### 错误日志

```json
{
  "timestamp": "2026-04-21T14:08:38+08:00",
  "command": "badcmd xyz",
  "cwd": "/home/user/project",
  "status": "error",
  "error_type": "runtime",
  "error_msg": "{code:\"nu::shell::external_command\",msg:\"External command failed\",help:\"`badcmd` is neither a Nushell built-in or a known external command\",labels:[[text,span,line,column];[\"Command `badcmd` not found\",badcmd,1,1]]}",
  "error_short": "External command failed: Command `badcmd` not found (`badcmd` is neither a Nushell built-in or a known external command)"
}
```

### 字段说明

| 字段 | 说明 |
|---|---|
| `timestamp` | 执行时间（RFC 3339） |
| `command` | 执行的 Nushell 源代码 |
| `cwd` | 命令执行时的工作目录 |
| `status` | `success` 或 `error` |
| `error_type` | `parse`（语法错误）、`compile`（编译错误）、`runtime`（运行时错误） |
| `error_msg` | 完整错误信息（NUON 格式，包含 code、msg、help、labels） |
| `error_short` | 简短错误描述（使用 Nushell 原生 ShortReportHandler 生成） |

## 使用技巧

### 按错误类型统计

```bash
cat nu_evo.jsonl | from json --objects | group-by error_type | values | each { { type: $in.0.error_type, count: ($in \| length) } }
```

### 查找最常见的错误

```bash
cat nu_evo.jsonl | from json --objects | where status == error | get error_short | frequencies | first 10
```

### 查看特定时间段的失败命令

```bash
cat nu_evo.jsonl | from json --objects | where status == error and timestamp > "2026-04-21"
```

### 分析某个目录下的错误

```bash
cat nu_evo.jsonl | from json --objects | where status == error and cwd =~ "my-project"
```

### 统计成功率

```bash
cat nu_evo.jsonl | from json --objects | group-by status | values | each { { status: $in.0.status, count: ($in | length) } }
```

### 与上游 Nushell 的区别

nushell-evo 与上游 [Nushell](https://github.com/nushell/nushell) 的区别：

- **MCP 命令执行日志**（`NU_MCP_LOG`）— 记录 AI 模型通过 MCP 执行的每条命令
- **内置 `browse` 插件**（[nu_plugin_browse](crates/nu_plugin_browse/README.md)）— headless 浏览器插件，支持 JS 执行、网络追踪、持久会话

#### MCP 日志

- MCP 工具名：`evaluate`（参数 `input`），`list_commands`，`command_help`
- 日志仅在 MCP 模式下生效，普通 REPL 不受影响
- 与 Nushell 内置的 `--log-level` 诊断日志是独立系统，互不重叠

#### browse 插件

需要系统安装 Chrome 或 Chromium。

```nu
# 一次性获取页面 HTML（ephemeral 浏览器，自动关闭）
browse https://example.com

# 执行 JavaScript（隔离世界）
browse https://example.com --eval "document.title"

# 执行 JavaScript（主世界，可访问页面变量）
browse https://example.com --real-eval "window.appState"

# 网络追踪（完整 headers + response body）
browse https://example.com --ntrace '.*'

# 持久浏览器（默认无头，跨调用复用 cookie/localStorage）
browse open https://example.com
browse open --eval "document.title"

# 持久浏览器 + 显示窗口
browse open https://example.com --with-head

# 查询持久浏览器状态
browse status
# => {status: "active", url: "...", session: "...", profile: "..."}

# 关闭持久浏览器
browse close
```

完整文档见 [crates/nu_plugin_browse/README.md](crates/nu_plugin_browse/README.md)。

## License

MIT（与上游 Nushell 保持一致）
