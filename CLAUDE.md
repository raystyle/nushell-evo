AGENTS.md

# nushell-evo 项目说明

基于 Nushell 的 fork，目标是通过记录大模型使用 Nushell 的命令执行情况（成功/失败），让大模型能够自我改进、更好地使用 Nushell，因此命名为 nushell-evo（evolution）。

## CI & Release

- CI 仅构建 Linux (ubuntu-22.04) + Windows，移除了 macOS 和 WASM 构建
- Release workflow (`release.yml`) 只构建 `x86_64-unknown-linux-gnu` 和 `x86_64-pc-windows-msvc`
- Tag 必须以 `v` 开头（如 `v0.1.0`）才会触发 release workflow
- Release 使用 `softprops/action-gh-release@v3`，需要 `permissions: contents: write`
- 安全审计使用 `actions-rust-lang/audit@v1`（替代已废弃的 `rustsec/audit-check`）
- **CLAUDE.md 不能是 symlink**，Windows GitHub Actions 无法创建 symlink（`120000` → `100644`）
- 推送 main 和推送 tag 会分别触发 CI 和 Release，建议先 push main 等 CI 通过后再 push tag

## MCP 命令执行日志

- `crates/nu-mcp/src/evaluation.rs` 实现了 MCP 模式下的命令执行审计日志
- 默认始终记录日志，保存到当前工作目录（CWD）下的 `nu_evo.jsonl`
- 可通过 `NU_MCP_LOG` 环境变量自定义日志路径（在 MCP 会话内通过 `$env.NU_MCP_LOG = "路径"` 设置）
- JSONL 格式，每条记录：`{timestamp, command, cwd, status, error_type?, error_msg?, error_short?}`
- 成功日志：`{timestamp, command, cwd, status: "success"}`
- 错误日志：`{timestamp, command, cwd, status: "error", error_type: "parse"|"compile"|"runtime", error_msg: "...", error_short: "..."}`
- `error_short` 使用 Nushell 原生 `ShortReportHandler` 生成，格式 `{diagnostic}: {label} ({help})`
- 与 Nushell 内置的 `--log-level` 诊断日志（Rust tracing）是不同用途，互不重叠
- MCP 工具名是 `evaluate`（不是 `eval`），参数名是 `input`（不是 `source`）

## 内置插件

- `nu_plugin_browse`（来源：[Tyarel8/nu_plugin_browse](https://github.com/Tyarel8/nu_plugin_browse)）— headless 浏览器插件，使用 `chaser-oxide`（CDP 协议），需要系统安装 chrome/chromium
- 使用方式：`cargo build -p nu_plugin_browse` 后运行 `plugin add target/debug/nu_plugin_browse && plugin use browse`
- 命令：
  - `browse <url>` — 一次性获取页面 HTML（ephemeral 浏览器，自动关闭）
  - `browse <url> --open` — 等价于 `browse open <url>`，打开持久浏览器
  - `browse open [url]` — 打开/连接持久浏览器（默认无头，`--with-head` 显示窗口），跨调用复用 cookie/localStorage
  - `browse close` — 关闭持久浏览器
- 参数：`--init-script <path>`（页面脚本前注入 JS）、`--eval <js>` / `-e`（隔离世界执行 JS，支持管道输入）、`--real-eval <js>`（主世界执行 JS）、`--no-stealth`、`--with-head`、`-w <duration>`、`--ntrace <pattern>`（网络追踪，默认含完整 headers 和 response body）
- Session 管理：`.nu_browse_profile/` 目录存储 Chrome profile 和 `.session` 文件，单 session 覆盖模式（`browse open` 自动回收旧 session）
- 错误处理：eval 错误返回 `{status: error, message: "eval error: ..."}` record，不会抛异常

## 开发经验

- `cargo build -p nu-mcp` 只编译 crate 本身，**MCP 功能集成在完整 binary 中**，需 `cargo build` 或 `cargo build --features=mcp` 才能测试 `nu --mcp`
- 修改 `eval_on_state` 等核心函数签名时，需同步更新 `eval_inner`、`promote_to_background_job`、测试代码中的 tuple 解构
- Nushell 的 `ErrorStyle::Short` 对应 `ShortReportHandler`（`crates/nu-protocol/src/errors/short_handler.rs`），通过 `Display` trait 使用，不能用 `Formatter::new()` 构造（unstable API），需用 wrapper struct 实现 `Display`
- `ShellError::from_diagnostic` 可将 `ParseError`/`CompileError` 转为 `ShellError`
- GitHub Actions 取消构建用 `gh run cancel <id>`，删除构建用 `gh run delete <id>`
