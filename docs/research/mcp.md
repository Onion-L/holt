# Coding Agent 的 MCP 接入模式调研

> 调研日期:2026-09-10。本文面向工程师,梳理主流 coding agent(含闭源但文档完备的 Claude Code / ZCode)如何接入 Model Context Protocol(MCP)并让 agent 使用 MCP 工具。背景:Holt 的 agent loop 来自 pi-core-rs,后者目前没有任何 MCP 支持,需要评估业界做法后再决定接入路径。所有结论尽量给到源码级证据;易变内容(版本号、默认值、行为开关)以查阅日期为准。
> 2026-09-21 增补:Zed(§8.12)与「配置互通:导入别人的 MCP 配置」(§10),并复核 Codex / Claude Code / pi 现状(过期项汇总见 §10.6);新增引用编号 [Z*]/[I*] 见 §12。

## 1. 结论先行

- **几乎人人都是 MCP client**:除 aider 明确不做外,所有被调研的 coding agent 都内建 MCP client;实现上优先选官方 SDK——TS 项目用 `@modelcontextprotocol/sdk`,Rust 项目(Codex、Goose、Amazon Q)用 `rmcp`,Go 项目 Crush 用 `github.com/modelcontextprotocol/go-sdk/mcp`,Python 的 Kimi CLI 用 `fastmcp`(包裹官方 `mcp` SDK)。自研协议栈的只有极个别早期实现,且都在向官方 SDK 迁移。
- **工具映射方式高度一致**:每个 MCP tool 被转换成本 agent 的一条普通 tool 定义(name + description + inputSchema + execute),走既有的 tool-call 循环,不为 MCP 单开执行通道。分歧只在命名空间:Claude Code / ZCode / Gemini CLI 用 `mcp__server__tool` / `mcp_server_tool` 全限定名,Codex / Kimi 直接用裸 `tool.name`(冲突时按服务端优先级或报错处理)。
- **transport 收敛为 stdio + Streamable HTTP**:spec 2025-03-26 起 Streamable HTTP 取代 HTTP+SSE;SSE 在 Claude Code 已标注 deprecated、在 Codex 源码中已删除。Claude Code 额外支持 WebSocket,Goose 支持 unix socket。
- **配置趋同于「JSON 里一坨 server 定义」**:Claude 系(`.mcp.json` 的 `mcpServers`)已成为事实标准——Codex(TOML 表达同样字段)、ZCode(`mcp.servers` + `.agents/mcp.json` 兼容回退)、Kimi、Continue、Amazon Q(源码注释自称 "mirror claude's config")都兼容或模仿这一形状。scope 层级普遍是 user / project(workspace)/ 本会话三层,外加 managed/插件层。
- **权限门控分三档**:server 级开关(enabled/disabled)、server 级审批模式(Codex 的 `default_tools_approval_mode`、Gemini 的 `trust`)、tool 级细粒度(Claude 的 `mcp__server__tool` 权限规则、Roo 的 `alwaysAllow`、Codex 的 per-tool `approval_mode`)。「首次使用项目级 server 需要用户确认」是 Claude Code 与 ZCode 共同的信任边界设计。
- **lazy loading 成为新战场**:Claude Code 默认开启 Tool Search(只注入工具名,按需取 schema);Codex 限制分页目录上限并缓存 catalog;Goose 提供 Code Mode(JS 内发现/调用工具)。这直接回应了 pi 作者的批评——大 MCP server 的工具定义动辄吃掉 7-9% 上下文。
- **Zed 走第三条路线:自研协议栈 + 裸工具名**(§8.12):手写 JSON-RPC/MCP 类型(不用 rmcp),stdio + streamable HTTP + 完整 OAuth(PKCE/DCR/keychain);LLM 看裸工具名、重名才加 server 前缀,settings/审批用稳定 ID `mcp:<server>:<tool>`;急切启动、失败手动重启、`tools/list` 不分页(已知短板);不读 `.mcp.json` 也不做导入。
- **「导入别人的配置」在生态里是显式动作**(§10):ZCode 扫四源勾选导入、Continue 目录直读 Claude 系 JSON、pi-mcp-adapter 显式 `/mcp setup`(host 发现默认 off);`.agents/mcp.json` 是无规范的事实约定,唯一有规范背书的是 Agent Plugins 1.0.0 的插件级 `mcp.json`。另:Codex 默认超时已改 30s/300s,crush 配置迁到 Bash `crushrc`。
- **双向能力常见但形态不同**:Claude Code(`claude mcp serve`)、Codex(已废弃的 `codex mcp-server`,继任者 app-server)能把自己暴露为 MCP server;Goose 只反向暴露内置扩展(`goose mcp <name>`),自身对外用 ACP;ZCode、Gemini CLI、Kimi 纯 client。
- **OAuth 已是 remote server 的标配**:普遍实现 OAuth 2.1 authorization-code + browser 回调,遵守 spec 2025-06-18 的 protected resource metadata / RFC 8707;动态客户端注册(DCR)或预注册 client_id 均有支持。

## 2. MCP 协议要点(与 coding agent 相关的部分)

### 2.1 版本与 transport 演进

MCP 用 JSON-RPC 2.0 编码消息,角色分 host(LLM 应用)、client(host 内的连接器)、server。[S1] 协议版本按日期命名,演进如下(查阅于 2026-09-10):

| 版本 | 关键变化 |
| --- | --- |
| 2024-11-05 | 首个稳定版;HTTP+SSE transport(两个端点) |
| 2025-03-26 | **Streamable HTTP 取代 HTTP+SSE**;移除 JSON-RPC batching |
| 2025-06-18 | 结构化工具输出(`structuredContent`/`outputSchema`);server 定位为 OAuth Resource Server;强制 RFC 8707 resource indicators;新增 elicitation;`MCP-Protocol-Version` 头 |
| 2025-11-25 | extensions 框架(SEP-2133)、异步 Tasks、工具名标准化格式(SEP-986)、图标元数据 |
| 2026-07-28 | 当前 latest;Tasks / MCP Apps(会话内交互式 UI)/ Skills over MCP 均以 opt-in extension 形式提供 [S2] |

当前 spec 只定义两种标准 transport [S3]:

- **stdio**:client 把 server 作为子进程启动,换行分隔的 JSON-RPC 走 stdin/stdout;stdout 只允许协议消息,日志走 stderr。所有被调研项目都支持。
- **Streamable HTTP**:server 提供单一 MCP endpoint;client 每条消息 POST,server 可返回单个 JSON 或开 SSE 流;支持 `Mcp-Session-Id` 会话头、`Last-Event-ID` 断线重放、`MCP-Protocol-Version` 头。旧 HTTP+SSE server 通过「POST 失败 4xx 后回退 GET 等 endpoint 事件」实现客户端兼容 [S3]。

MCP Apps(把带 UI 的交互组件渲染进对话)在 2026-07-28 spec 中是 extension;Goose、Claude 生态已跟进相关能力。[S2]

### 2.2 tools 原语

- `tools/list` 支持游标分页;`tools/call` 携带 `name` + `arguments`;`isError: true` 表示工具执行错误(区别于 JSON-RPC 协议错误)。[S4]
- server 在 initialize 时声明 `tools.listChanged` 能力,工具列表变化时发 `notifications/tools/list_changed`,client 应重新拉取——Claude Code、ZCode 等都据此做动态刷新。[S4]
- tool 定义含 `name`/`title`/`description`/`inputSchema`/`outputSchema`/`annotations`;**spec 明确 annotations(readOnlyHint 等)不可信**。tool 结果可为 `content`(text/image/audio/resource_link/embedded resource)加 `structuredContent`。[S4]
- 安全建议:人类在环 SHOULD、调用前展示工具输入、结果先验证再喂给 LLM、实现调用超时。[S4]

### 2.3 OAuth

2025-06-18 起,server 是 OAuth 2.1 下的 Resource Server,通过 protected resource metadata 指向 authorization server;client 必须实现 RFC 8707 resource indicators 防止 token 被恶意 server 骗取。[S5] 各家 client(Claude、Codex、Goose、opencode、Crush、Kimi、Gemini CLI)都实现了「401 → 发现授权元数据 → 浏览器授权 → 本地回调拿 code → 存 token → 自动刷新」的流程,差异在回调端口管理(固定/随机)和客户端注册方式(DCR / CIMD / 预注册)。

## 3. OpenAI Codex CLI

Rust,openai/codex,~122.8k stars(2026-09-10)。[S6]

### 3.1 SDK 与内部结构

- 使用官方 Rust SDK:**`codex-rmcp-client` crate 依赖 `rmcp`**,features 覆盖 `client`、`server`、`transport-child-process`、`transport-streamable-http-client/server`、`auth`,并配 `oauth2` crate。[S7] 早期的 `experimental_use_rmcp_client` 开关已从代码库中消失(github code search 无结果)——rmcp 已是唯一实现,不再"实验"。
- MCP 相关代码集中在 `codex-rs/codex-mcp`(连接管理、catalog、elicitation、OAuth)、`codex-rs/rmcp-client`、`codex-rs/config/src/mcp_types.rs`。[S6][S8]

### 3.2 配置(`config.toml`)

`config.toml` 顶层 `mcp_servers: HashMap<String, McpServerConfig>`。[S9] 源码里 `RawMcpServerConfig`(deny_unknown_fields)的字段全集:[S8]

```toml
[mcp_servers.chrome_devtools]
url = "http://localhost:3000/mcp"      # 或 command/args/env(stdio)
enabled_tools = ["open"]                # 白名单,先应用
disabled_tools = ["screenshot"]         # 黑名单,后应用
default_tools_approval_mode = "prompt"  # auto | prompt | writes | approve
startup_timeout_sec = 20                # 默认 10
tool_timeout_sec = 45                   # 默认 60
enabled = true
required = true                         # 初始化失败则整体启动失败
[mcp_servers.chrome_devtools.tools.open]
approval_mode = "approve"               # per-tool 审批
output_token_limit = 30000              # per-tool 输出截断
```

其他字段:`env_vars`(环境变量引用)、`bearer_token_env_var`、`http_headers`/`env_http_headers`/`http_headers_helper`(本地 shell 命令动态生成头)、`supports_parallel_tool_calls`、`scopes`、`oauth { client_id, callback_url, callback_port }`、`oauth_resource`。全局有 `mcp_oauth_credentials_store`、`mcp_oauth_callback_port/url`、`mcp_optional_startup_grace_ms`(默认 1000ms,等待可选 server 的宽限期)。[S8][S9]

> 2026-09-21 复核:默认超时已改为 startup 30s / tool 300s,`auth` 枚举增加 `ema_auth`,新增 `startup_timeout_ms`、`environment_id`、`omit_tools_from` 等字段;transport 枚举不变。详见 §10.2。[I3][I4][I5]

### 3.3 transport 与审批

- transport 枚举**只剩 `Stdio` 和 `StreamableHttp`** 两个变体,SSE 已从源码删除。[S10]
- 审批模型 `AppToolApproval = auto | prompt | writes | approve`,带保守合并语义(`restrict_to`):server 级默认值与 per-tool 值合并时,任一是 `prompt` 即 `prompt`。[S8] 这与 Codex 自身的 `approval_policy`(untrusted/on-request/never 等)是两套正交机制——MCP 工具审批独立于 shell 命令审批。

### 3.4 工具暴露给模型

- **裸工具名**:`parse_mcp_tool` 直接用 `tool.name.to_string()`,不加 server 前缀;跨 server 的同名冲突在 catalog 层按同层优先级规则处理("A same-tier name collision and the final outcome after all precedence is applied")。[S11]
- 为兼容 OpenAI 模型,若 inputSchema 缺 `properties` 会补一个空对象;agent 插件场景下 description 截断到 1000 字节。[S11]
- `tools/list` 遵循分页,上限 100 页 / 每 server 2048 个工具,并缓存 tool catalog(`McpToolCatalogCache`),启动时按 `mcp_optional_startup_grace_ms` 给可选 server 宽限。[S12][S9]

### 3.5 CLI 与 OAuth

`codex mcp` 子命令组:`list`、`get`、`add`、`remove`、`login`、`logout`(源码枚举)。[S13] OAuth 支持 DCR 与 CIMD,`codex mcp login <name> --oauth-client-registration cimd|dcr|auto`;HTTP server 的 `auth = "oauth" | "chatgpt"`(后者仅限 ChatGPT 第一方域名)。[S8][S9]

### 3.6 反向:MCP server 模式

`codex mcp-server`(stdio)已**废弃**,官方建议改用 Codex app server 或面向 Claude Code 的 Codex 插件;该模式下仅暴露两个工具:`codex`(带 prompt/approval-policy/sandbox/config 等参数,一次调用即一个会话)和 `codex-reply`。[S14] 当前 main 分支 CLI 的子命令表里已无 `mcp-server`,取而代之的是 `[experimental] app-server`。[S15]

## 4. Claude Code 与 claude-agent-sdk

CLI 本体闭源;anthropics/claude-code 仓库只是文档/issue。文档站 code.claude.com,以下查阅于 2026-09-10。

### 4.1 配置与 scope

- CLI:`claude mcp add --transport http|sse|stdio <name> ...`、`add-json`、`list`、`get`、`remove`、`login/logout`、`reset-project-choices`;会话内 `/mcp` 面板。[S16]
- 三个 scope:local(默认,存 `~/.claude.json` 的项目条目下)、project(`.mcp.json`,进版本库)、user(`~/.claude.json` 顶层)。同名冲突**整体取高优先级定义、不做字段级合并**,顺序 local > project > user > 插件 > claude.ai connectors;组织 `managedMcpServers` 最高。project scope 的 server 首次使用要交互确认(信任边界)。[S17]
- `.mcp.json` 支持 `${VAR}` 与 `${VAR:-default}` 展开(command/args/env/url/headers)。[S17]

### 4.2 transport

http(推荐,JSON 里 `streamable-http` 是 `http` 的别名)、sse(已 deprecated)、stdio(`--` 分隔参数,server 侧可读 `CLAUDE_PROJECT_DIR`)、ws(仅 JSON 配置,不支持 OAuth)。url 无 type 的条目按 stdio 解析并报错跳过。[S16]

### 4.3 工具命名与权限

- 工具名 `mcp__<server>__<tool>`;插件 server 为 `mcp__plugin_<plugin>_<server>__<tool>`,非法字符替换为 `_`。[S18]
- 权限规则:`mcp__server`(整个 server)、`mcp__server__tool`(精确)、`mcp__server__*`/`mcp__server__prefix_*`(allow 规则的 glob 必须带字面 `mcp__<server>__` 前缀)、`mcp__*`(deny 所有 MCP 工具)。allow 规则不支持裸 `mcp__*`。tool 的 `_meta["anthropic/requiresUserInteraction"] = true` 可强制每次弹审批,连 bypassPermissions 也拦。[S19][S20]

### 4.4 Tool Search / 延迟加载

默认开启:会话开始只注入工具名与 server instructions,模型用 `ToolSearch` 工具按需取定义(需要支持 `tool_reference` 的 Claude 4.5+ 模型)。`ENABLE_TOOL_SEARCH` 可设 `true`/`auto`(定义总量 < 10% 上下文则全部前置加载)/`auto:N`/`false`;server 级 `alwaysLoad: true` 或 tool 级 `_meta anthropic/alwaysLoad` 豁免。tool description 与 server instructions 截断到 2KB。无固定 per-server 工具数量上限。[S21]

### 4.5 超时、输出与生命周期

- 启动超时 `MCP_TIMEOUT`;per-server `timeout`(ms)是每次调用的硬墙钟上限;`MCP_TOOL_TIMEOUT` 未设时默认约 28 小时;空闲超时 `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`(remote 5 分钟、stdio 30 分钟);主会话中超过 2 分钟的 MCP 调用自动转后台任务。[S22]
- 输出超 10k token 警告,默认上限 25k(`MAX_MCP_OUTPUT_TOKENS`);tool `_meta["anthropic/maxResultSizeChars"]` 可放宽到 50 万字符,超限结果落盘换成文件引用。[S23]
- client runtime: v1 = MCP TS SDK 1.x,v2 = SDK 2.0(支持 2026-07-28 协议修订);支持 `list_changed` 动态刷新与 remote server 断线指数退避重连(stdio 不重连)。[S24]

### 4.6 tools 之外的能力

- **prompts → slash 命令**:`/mcp__server__promptname (MCP)`,参数空格分隔。[S25]
- **resources → @ 引用**:`@server:protocol://path` 自动抓取为附件。[S25]
- **elicitation**:form 模式弹对话框、URL 模式开浏览器;可用 Elicitation hook 自动应答。[S25]
- **roots**:`roots/list` 返回启动目录 + 额外工作目录,变化时发 `notifications/roots/list_changed`。[S16]
- **plugins 打包 MCP**:插件根 `.mcp.json` 或 `plugin.json` 内联 `mcpServers`,支持 `${CLAUDE_PLUGIN_ROOT}` 占位;启用插件即自动连接。[S18]

### 4.7 反向与 SDK

`claude mcp serve` 把 Claude Code 自身变成 stdio MCP server,只暴露 Claude Code 的工具(审批由外部 client 负责)。[S26] claude-agent-sdk(TS/Python,开源)内嵌同一套能力:`query({ options: { mcpServers: {...}, allowedTools: ["mcp__server__*"] } })`,除 stdio/http/sse 外还支持 SDK 进程内 server(`createSdkMcpServer`)。[S27]

## 5. Kimi CLI

MoonshotAI/kimi-cli,Python,~11.3k stars(2026-09-10)。[S28]

- **SDK**:`fastmcp==3.2.4`(pyproject 依赖),类型来自官方 `mcp` 包;client 逐 server 创建 `fastmcp.Client`。[S29]
- **配置**:`~/.kimi/mcp.json`,顶层 `mcpServers`,与 Claude 系格式兼容;另有 `kimi --mcp-config-file <path>` / `--mcp-config '<json>'` 临时注入。`kimi mcp add --transport http|stdio`、`list`、`remove`、`auth`、`test` 子命令。[S30]
- **transport**:stdio + streamable HTTP;HTTP 支持 `--header` 与 `--auth oauth`(浏览器授权,token 存 `~/.kimi/mcp-oauth/`)。[S30]
- **工具暴露**:**裸 `tool.name`**,server 名写进 description 前缀("This is an MCP (Model Context Protocol) tool from MCP server `X`"),schema 直接透传 `inputSchema`;工具注册进统一 toolset 后走正常 agent loop。[S29]
- **审批与超时**:所有 MCP 工具调用默认走 `runtime.approval.request` 人工确认,YOLO/AFK 模式下自动放行;per-call 超时来自 `mcp.client.tool_call_timeout_ms`。[S29]
- **生命周期**:shell UI 启动后异步连接,状态栏实时显示;支持延迟加载(`defer_mcp_tool_loading`)。会话内 `/mcp` 查看状态。[S30]

## 6. ZCode

Z.ai 的 coding agent(桌面 app + CLI),**闭源**,GitHub 上只有插件市场 `zai-org/zcode-plugins`。以下结合本机一手文件(`~/.zcode/cli/`,查阅 2026-09-10)与官方文档 zcode.z.ai。[S31][S32]

- **配置与 scope**(本机 `zcode-guide` 插件 SKILL.md,一手):user `~/.zcode/cli/config.json` → `mcp.servers`;workspace `<repo>/.zcode/config.json` 或 `zcode.json` → `mcp.servers`;每 scope 的 `.agents/mcp.json`(顶层 `mcpServers`)仅作同 scope 兜底。同名覆盖顺序 **CLI → 环境变量 → user → workspace → system**,user 压过 workspace。所有 scope 的 server **会话启动时自动信任并连接**(早期 workspace 需手动授权,现已改)。桌面端以 Settings → MCP 为状态/修复入口。[S31]
- **schema**:stdio `{command, args, cwd, env, enabled, timeoutMs}`;http/sse `{url, headers, enabled, timeoutMs}`;`type` 可由 command/url 推断;**schema 严格,未知键整个 server 被丢弃**;`${...}` 模板变量仅插件 server 展开;默认超时 30000ms。[S31]
- **插件 server**:插件根 `.mcp.json` 或 manifest `mcpServers` 字段(本机 computer-use 插件实锤:`.zcode-plugin/plugin.json` 定义 stdio server,由 ZCode Helper 以 `zcode.cjs __zcode-plugin-host <server.js>` 承载),server 注册名为 `plugin:<plugin>:<server>`。[S31][S33]
- **工具命名**:`mcp__<server>__<tool>`(本机日志实锤:`mcp__langchain_docs__query_docs_filesystem_docs_by_lang_chain`)。[S34]
- **运行时行为**(日志实锤):启动事件 `mcp.startup.completed`(serverCount/statusCounts/toolCount)、`mcp.tools.registered`(registeredToolCount);连接经 `adapters.mcp.pool` 池化,`mcpIsolation: "session"`(每会话隔离);插件 server 配置 `timeoutMs: 90000`。[S34]
- **OAuth**:仅 http/sse 远程 server;点「Open authorization」走系统浏览器,文档强调 "ZCode never opens a browser window on its own"。[S32]
- **其他**:支持从 Claude Code / Codex / OpenCode / 通用 `.agents/mcp.json` 一键导入;权限走 `tool.permission.resolved`(mode 含 yolo 等);无「自身作为 MCP server」的文档或迹象。[S32][S34]

## 7. pi 与 pi-core-rs

### 7.1 TS 侧(badlogic/pi-mono → earendil-works/pi,~103k stars)

- 全树检索 `mcp`(大小写不敏感)为 **0 命中**:没有 `@mariozechner/pi-mcp` 之类的包,npm 上也不存在。pi 通过 extension 系统扩展工具,而不是内置 MCP。[S35]
- 作者 Mario Zechner 在博客《What if you don't need MCP at all?》给出设计立场:通用 MCP server 为"覆盖所有场景"堆工具、描述冗长(实测 Playwright MCP 13.7k token ≈ 6.8% 上下文、Chrome DevTools MCP 18k ≈ 9%),扩展性和可组合性差;他更倾向小 CLI 脚本 + skills 的渐进披露路线,并承认代价是"工具维护要自己扛"。[S36]

- 2026-09-21 复核:pi(TS)与 pi-core-rs 仍无内置 MCP;但生态出现第三方扩展 `pi-mcp-adapter`(lazy 连接 + 单代理工具省 context,原生读 `.mcp.json` 与 `~/.config/mcp/mcp.json`,host 配置导入走显式 `/mcp setup` 且自动发现默认 off)——官方「不用 MCP」的路线未变,MCP 由扩展层承接。[I14]

### 7.2 Rust 侧(pi-core-rs,Holt 的 fork 源)

- `Onion-L/pi-core-rs`("A standalone Rust runtime for Pi's agent, AI, and telemetry core")全树 329 个文件中 **无任何 mcp 命名文件**;crate 分层为 `agent`/`ai`/`bin`/`telemetry`。[S37]
- 工具抽象是纯数据 + 闭包的 `AgentTool`:`{ name, label, description, parameters: serde_json::Value(JSON Schema), execute: Arc<dyn Fn(&str, &Value, ...) -> BoxFuture<Result<AgentToolResult, String>>> }`;harness 内置 bash/read/write/edit 就是按这个结构实现的。**没有任何 MCP 客户端**。[S37]

### 7.3 对 Holt 的含义

pi-core-rs 不提供 MCP,意味着接入点完全在 Holt 侧;而 `AgentTool` 恰好等价于其他项目把 MCP tool 降维后的形态(name/description/JSON schema/execute 闭包),见 §10。

## 8. 其他项目概览

### 8.1 google-gemini/gemini-cli(TS,~106.9k stars)

`settings.json` 顶层 `mcpServers`(`command`|`url`(SSE)|`httpUrl` 三选一,另有 `headers`/`env`/`cwd`/`timeout` 默认 10 分钟/`trust` 跳过确认/`includeTools`/`excludeTools`)+ 全局 `mcp.allowed/excluded`。基于 `@modelcontextprotocol/sdk`;工具注册为 FQN **`mcp_<server>_<tool>`**(单下划线,文档警告 server 名里不要用下划线,否则权限规则解析错位);`/mcp` 展示 tools/prompts/resources,`/mcp auth` 管理 OAuth(401 触发自动发现 + 浏览器流);schema 会做 Gemini API 兼容性清洗(去 `$schema`、`additionalProperties` 等)。[S38]

### 8.2 QwenLM/qwen-code(TS,~27.7k stars)

gemini-cli fork;`packages/core/src/tools/` 下同样存在 `mcp-client.ts`/`mcp-tool.ts` 全套(并演进出 transport pool、session 视图等 fork 特有文件),`mcpServers` 配置机制与 gemini-cli 同源。[S39]

### 8.3 cline/cline(VS Code 扩展,~67.7k stars)

依赖 `@modelcontextprotocol/sdk@^1.25.1`;`apps/vscode/src/services/mcp/McpHub.ts` 统一管理连接,transport 三件套 `StdioClientTransport`/`SSEClientTransport`/`StreamableHTTPClientTransport`。配置在全局存储的 `cline_mcp_settings.json`(常量 `CLINE_MCP_SETTINGS_FILE_NAME`,chokidar watch 热重载);per-tool auto-approve(`toggleToolAutoApprove` 控制器);带 MCP marketplace、remote MCP 与 `McpOAuthManager` OAuth;CLI 版提供 `cline mcp`。[S40]

### 8.4 RooCodeInc/Roo-Code(TS,~24.3k stars)

Cline fork;同样的 `McpHub.ts` 与三种 transport;差异点在配置 schema 内建 `alwaysAllow: z.array(z.string())` **per-tool 永久放行列表**,以及独立的 `McpServerManager` 多窗口共享。[S41]

### 8.5 block/goose(Rust,~54.1k stars,现 aaif-goose/goose)

MCP-first:"extensions" 就是 MCP server,内置 Developer/Memory/Computer Controller 等扩展默认启用,装扩展前自动查恶意包。Rust 侧用 **rmcp**(`transport-child-process`、`transport-streamable-http-client(-reqwest)`、`transport-streamable-http-client-unix-socket`)+ `oauth2`。`goose configure` → Add Extension,或 `goose mcp {name}`(注意:这是运行 goose **内置**的 MCP server——源码枚举只有 AutoVisualiser/ComputerController/Memory/Tutorial——而非把自己整体作为 server);对外作为 agent 时走 ACP(`goose acp`,stdio)。权限模式:Completely Autonomous(**默认**)/ Manual Approval / Smart Approval(低风险自动批)/ Chat Only,外加 per-tool permissions。[S42]

### 8.6 sst/opencode(TS,~206.1k stars,现 anomalyco/opencode)

`opencode.json` 顶层 `mcp`,`type: "local"`(`command` 为**数组**、`environment`、`timeout` 默认 5000ms)或 `"remote"`(`url`、`headers`、`oauth` 可设 false 关闭自动探测);OAuth 走 DCR,默认回调 `http://127.0.0.1:19876/mcp/oauth/callback`。早期用 Vercel AI SDK 的 `experimental_createMCPClient`,现源码已直接 import `@modelcontextprotocol/sdk` 的三种 transport。[S43]

### 8.7 charmbracelet/crush(Go,~28k stars)

README 宣传 stdio/http/sse 三种 transport;配置在 `crush.json` 的 `mcp` 键或 `mcp add` 命令(`--command/--args/--timeout/--disabled-tools/--header`),支持 `oauth: true` 的 authorization-code 流与预注册 client。源码使用**官方 Go SDK** `github.com/modelcontextprotocol/go-sdk/mcp`(不是早前第三方 mcp-go),`internal/agent/tools/mcp/` 负责生命周期 reconcile 与工具注入,并实现了 channel 能力(server 可向会话推送事件)。[S44] 2026-09-21 注:crush 配置体系已重写为 Bash 式 `crushrc`(`mcp add` 子命令),JSON `mcp` 键标 deprecated、不再收新特性,见 §10.3。[I12]

### 8.8 All-Hands-AI/OpenHands(~87.1k stars)

项目已重写为 TypeScript/Electron 桌面应用;MCP server 在设置 UI 管理(`src/routes/mcp-settings.tsx`,specs/mcp-settings.md 规定稀疏变更、密钥保留等持久化语义)。历史上 Python 版在 `config.toml` 的 `[mcp]` 段配置 sse/http 远程与 stdio 本地 server,并把 MCP 工具转成 agent action——该路径随重写演进,细节见官方文档。[S45]

### 8.9 Aider-AI/aider(Python,~48.9k stars)

**明确不支持 MCP 的反例**。README 无 MCP 集成;feature request issue #2525("Please add support for model context protocol")与 #3314("MCP SUPPORT")长期 open,社区 PR(#3672、#3937)未合并;用户靠 mcpm-aider、AiderDesk 等外部桥接。其默认工具集(shell/file 编辑/内建 web)自足,作者未接受 MCP 路线。[S46]

### 8.10 continuedev/continue(TS,~35.8k stars)

`config.yaml` 顶层 `mcpServers` 列表,或 workspace `.continue/mcpServers/*.yaml` 独立 block(需 `name`/`version`/`schema` 元数据);**直接接受 Claude Desktop/Cursor/Cline 的 JSON `mcpServers` 文件**放进该目录。type 支持 `stdio`/`sse`/`streamable-http`。MCP 仅在 agent 模式生效。[S47]

### 8.11 aws/amazon-q-developer-cli(Rust,~2k stars)

`crates/agent/src/agent/mcp` 用 **rmcp** 实现 client(`rmcp::Service<RoleClient>`);agent 配置 JSON 内嵌 `mcpServers` 字段(源码注释原文:"This is to mirror claude's config set up"),也支持独立 `mcp.json`(顶层 `mcpServers`)与 OAuth(`mcp_client/oauth_util.rs`)。工具准入靠 agent 的 **`allowedTools`** 集合做 allowlist(配合 server 名判定 MCP 工具引用)。[S48]

### 8.12 zed-industries/zed(Rust,开源)

> 2026-09-21 查阅,源码锚定 `main@85b8198e`,zed.dev 文档同日;引用 [Z*] 见 §12。

- **配置**:`context_servers` 键(project settings 命名空间,user `settings.json` 与项目 `.zed/settings.json` 同 schema),`#[serde(untagged)]` 三形态:stdio / http / extension(由扩展提供启动命令)。[Z2]

```jsonc
{
  "context_servers": {
    "local-server": {
      "enabled": true,                        // 默认 true
      "command": "npx",
      "args": ["-y", "some-mcp-server"],
      "env": { "KEY": "value" },              // 增量合入继承的进程环境
      "timeout": 120,                         // 秒;工具调用超时,覆盖全局
      "remote": false                         // 默认 false;true 时远程开发场景跑在远端
    },
    "remote-server": {
      "enabled": true,
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" },
      "oauth": { "client_id": "...", "client_secret": "..." }  // 预注册凭据(server 不支持 DCR 时)
    },
    "extension-server": { "enabled": true, "settings": { } }   // 扩展定义的自由 JSON
  },
  "context_server_timeout": 60               // 全局默认调用超时(秒)
}
```

- 超时链:per-server `timeout` > 全局 `context_server_timeout`(默认 60s),clamp 上限 600s。[Z2][Z3] UI 入口 Settings → AI → MCP Servers(`Add Local/Remote Server`、`Install from Extensions`),写入同一 schema。[Z19] 扩展形态的 MCP server 已标注计划弃用,转向官方 MCP registry(issue #59351)。[Z18]
- **不读 `.mcp.json`,也没有任何从其他工具导入配置的机制**(全仓库 grep 零命中;官方文档明确 external agent 自己读自己的原生 MCP 配置,Zed 只转发自己配的 server)。[Z17]
- **实现自研**:`crates/context_server` 手写 JSON-RPC 2.0 + MCP 类型,不依赖 rmcp / 官方 SDK;协商 2024-11-05 ~ 2025-11-25 四个协议版本。[Z4][Z6][Z8] transport 仅 stdio + streamable HTTP(单端点 POST、SSE 响应流、`Mcp-Session-Id` 会话、销毁发 `DELETE`、≥2025-06-18 附 `MCP-Protocol-Version` 头),无 legacy SSE GET。[Z5]
- **OAuth 完整**:401 → RFC 9728 resource metadata → discovery → DCR(支持 CIMD,settings 可预注册 `client_id`)→ loopback 回调 + PKCE → token 存系统 keychain 并自动刷新;initialize 之后的 401 也由 transport shutdown 监听捕获转入重新授权。[Z9]
- **工具暴露**:server 进入 `Running` 即 `tools/list`,每个 MCP tool 包成 `ContextServerTool`。LLM 看到**裸工具名**(清洗 `[A-Za-z0-9_-]`、≤64 字符),仅跨 server 或与内置工具重名时加 `<server_snake>_<tool>` 前缀;settings/审批用稳定 ID `mcp:<server>:<tool>`。profile 级开关 `agent.profiles.<id>.enable_all_context_servers`(默认 write 全开、ask 全关)。[Z10][Z11][Z15]
- **审批**:默认 `confirm`(v0.224.0 起由 `agent.tool_permissions.default` 控制);MCP 工具只支持 per-tool `default: allow|deny|confirm`,不支持输入正则;agent panel 下拉审批卡("Always for X MCP tool" 持久化写 settings)。[Z12][Z13][Z14][Z20]
- **生命周期**:随 Project 创建急切启动全部 enabled server;settings/扩展/worktree 变更触发 reconcile(配置变→重启、删除→移除、`enabled:false`→停但保留);失败进 Error **无自动重试**,靠 UI 手动 Restart;状态机 Starting/Running/Stopped/Error/AuthRequired/ClientSecretRequired/Authenticating,状态点在 Settings → AI → MCP Servers;stdio 进程非 auth 原因死亡不改状态(源码注释承认的坑)。[Z3][Z19]
- `tools/list_changed` 有订阅(全量重拉、下一轮生效),但 **`tools/list` 不跟随 `next_cursor` 分页**——超过一页的工具会丢,已知短板。[Z10][Z11]
- **反向:不把自己暴露为 MCP server**(`context_server` crate 只有 client 侧);对外集成走 ACP:`newSession` 时把自己配置的 MCP server 转发给 external agents,Zed Agent 自身也可作 ACP agent server 运行。[Z4][Z16][Z21][Z22]

## 9. 横向对比

| 项目 | 语言 | SDK | 配置方式 | transport | 工具命名 | 权限粒度 | OAuth | lazy loading | 反向 MCP server |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Codex CLI | Rust | rmcp(官方) | `config.toml` `mcp_servers` | stdio, Streamable HTTP(SSE 已删) | 裸 `tool.name` | server 审批模式 + per-tool approval/output 限额 | DCR/CIMD/chatgpt,`mcp login` | catalog 缓存 + 分页上限 + 启动宽限 | 已废弃 `mcp-server` → app-server |
| Claude Code | 闭源 | MCP TS SDK 1.x/2.0 | `claude mcp add` + `.mcp.json`,3 scope + 插件/托管 | stdio, http, sse(弃), ws | `mcp__server__tool` | server/tool 正则规则 + requiresUserInteraction | `/mcp` + `mcp login`,自动刷新 | Tool Search 默认开,`auto:N` 阈值,`alwaysLoad` | `claude mcp serve` |
| Kimi CLI | Python | fastmcp(官方 mcp 包装) | `~/.kimi/mcp.json` + `kimi mcp add` | stdio, Streamable HTTP | 裸 `tool.name`(server 进 description) | 全部调用弹审批;YOLO/AFK 放行 | `--auth oauth`,`kimi mcp auth` | UI 后异步加载,可延迟 | 无 |
| ZCode | 闭源 | 未公开 | `~/.zcode/cli/config.json` / `.zcode/config.json` `mcp.servers` + `.agents` 兜底 + 插件 | stdio, http, sse | `mcp__server__tool`(日志实锤) | 自动连接 + Settings 审计;mode 权限 | 浏览器授权按钮 | 会话级连接池,启动汇总 toolCount | 无(纯 host) |
| pi / pi-core-rs | TS/Rust | 无 | 无 | 无 | — | — | — | — | 无 |
| Zed | Rust | 自研(`crates/context_server`) | settings.json `context_servers` + Settings UI | stdio, Streamable HTTP | 裸名(冲突加前缀);权限 ID `mcp:<server>:<tool>` | per-tool allow/deny/confirm | PKCE+DCR/CIMD,keychain | 无(不分页、全量注入) | 无(对外走 ACP) |
| gemini-cli | TS | @modelcontextprotocol/sdk | `settings.json` `mcpServers` | stdio, SSE, Streamable HTTP | `mcp_server_tool` FQN | `trust` + include/excludeTools + Policy Engine | 自动发现,`/mcp auth` | 无(全量注入) | 无 |
| qwen-code | TS | 同 gemini | 同 gemini(fork) | 同 gemini | 同 gemini | 同 gemini | 同 gemini | 无 | 无 |
| cline | TS | @modelcontextprotocol/sdk | `cline_mcp_settings.json` + marketplace | stdio, SSE, Streamable HTTP | server 名 + 工具名 | per-tool auto-approve | McpOAuthManager | 无 | 无 |
| Roo-Code | TS | @modelcontextprotocol/sdk | 同 cline(fork) | 同 cline | 同 cline | per-tool `alwaysAllow` | 同 cline | 无 | 无 |
| goose | Rust | rmcp(官方) | `goose configure` + config | stdio, Streamable HTTP, unix socket | extension:tool | 四种模式(默认全自动)+ per-tool | oauth2 crate | Code Mode(JS 动态发现) | 内置扩展可作 server(`goose mcp <name>`) |
| opencode | TS | @modelcontextprotocol/sdk(自 AI SDK 迁来) | `opencode.json` `mcp`(local/remote) | stdio, SSE, Streamable HTTP | server 名 + 工具名 | enabled 开关 + 审批体系 | DCR,回调 19876 | 无 | 无 |
| crush | Go | modelcontextprotocol/go-sdk(官方) | `crush.json` `mcp` + `mcp add` | stdio, http, sse | server 名 + 工具名 | `disabledTools` + 审批 | `oauth: true` + 预注册 | 无 | 无 |
| OpenHands | TS(重写后) | 未逐项核验 | 设置 UI(原 config.toml `[mcp]`) | sse, http, stdio(历史) | server + 工具 | 设置 UI 管理 | 有(历史文档) | 无 | 无 |
| aider | Python | 无 | 无 | 无 | — | — | — | — | — |
| continue | TS | 官方 SDK | `config.yaml` / `.continue/mcpServers/*.yaml` + 兼容 JSON | stdio, sse, streamable-http | server 名 + 工具名 | agent 模式权限 | — | 无 | 无 |
| Amazon Q CLI | Rust | rmcp(官方) | agent JSON `mcpServers` + `mcp.json`("mirror claude") | stdio, http | server + 工具 | `allowedTools` allowlist | oauth_util | 无 | 无 |

## 10. 配置互通:导入别人的 MCP 配置(2026-09-21 增补)

> 本节回答「把别的工具里已经配好的 MCP server 搬进 Holt」。核对基线为本文 2026-09-10 快照;引用 [I*] 见 §12。

### 10.1 Claude Code:schema 复核(快照准确但不完整)

- 存放:local `~/.claude.json` 的 `projects["<abs-path>"].mcpServers`;project 仓库根 `.mcp.json`;user `~/.claude.json` 顶层 `mcpServers`。`~/.claude/settings.json` **没有** `mcpServers` 键——ZCode 文档声称从 `~/.claude/settings.json` 导入 Claude 配置,路径写错了;做导入器以 `~/.claude.json` + `.mcp.json` 为准。[I1][I2][I6]
- 条目字段:stdio `command`(判别依据)/`args`/`env`,`type:"stdio"` 可缺省;http 需 `type:"http"`(`streamable-http` 为别名)+ `url`,可选 `headers`、`headersHelper`(shell 命令动态生成)、`timeout`(ms,per-call)、`alwaysLoad`、`oauth{clientId,callbackPort}`(camelCase);sse 同 http(弃用);ws 仅 JSON 可配。**有 `url` 无 `type` 的条目整条跳过**。[I1]
- `${VAR}`/`${VAR:-default}`:作用于 command/args/env/url/headers;未设且无默认 → 保留字面量 + 警告;remote url/headers 中的凭证类变量(`ANTHROPIC_API_KEY`、`HTTPS_PROXY` 等)读作空串且 `:-default` 被忽略(防配置文件外泄宿主凭证);展示面始终显示未展开引用。[I1]
- server 名限制 `[A-Za-z0-9_-]`,保留名加载时跳过;Claude Code 自身的导入能力只有 `add-from-claude-desktop`(交互勾选)+ `add-json` 人工转换。[I1]

### 10.2 Codex:字段集变化(快照部分过期)

- **默认超时 10s/60s → 30s/300s**(`DEFAULT_STARTUP_TIMEOUT` / `DEFAULT_TOOL_TIMEOUT`)。[I5]
- RawMcpServerConfig 新增/变化:`auth` 枚举加 `ema_auth`(企业 IdP);`oauth` 加 `authorization_server_issuer`;新增 `startup_timeout_ms`(sec 优先)、`environment_id`(默认 `"local"`)、`omit_tools_from`(按模型可见面屏蔽工具)、`name`(legacy 兼容)、`bearer_token`(显式接受但必报错防误配);`env_vars` 升级为结构化 `{name, source: local|remote}`;全局键新增 `mcp_enterprise_managed_auth` 与 `[orchestrator] mcp.enabled`。transport 枚举仍只 Stdio + StreamableHttp(快照正确)。[I3][I4]
- 对互通影响小(新字段皆可忽略项),但引用旧默认值的地方要改。

### 10.3 各家「导入别人的配置」现状(三档)

- **主动扫描 + 勾选导入**:ZCode(Settings → MCP Servers → Import,扫 Claude Code / Codex / OpenCode / `.agents/mcp.json` 四源,写入原生配置、源文件不动、导入后默认 enabled)[I6];pi-mcp-adapter(`/mcp setup` 展示 → 勾选 → 预览 → 写入,host 配置自动发现默认 `"off"`,从不回写外部文件、不静默执行其中命令)[I14]。
- **文件级直读**:Continue 把 Claude Desktop / Cursor / Cline 的 `mcpServers` JSON 直接拷进 `.continue/mcpServers/` 即自动加载[I8];ZCode / pi-mcp-adapter 读 `.agents/mcp.json` 兜底;kimi 的 `--mcp-config-file <path>` 可加载任意位置的 mcpServers JSON(其 remote 条目只有 `url` 无 `type` 也按 http 解析,与 Claude 相反)[I10]。
- **只进不出 / 无导入**:opencode(组织可经 `.well-known/opencode` 下发默认 remote server)[I9]、crush、gemini-cli、Claude Code(仅收 Claude Desktop)。没有一家做「静默全量读别人配置」——显式、用户可见是主流安全姿态。
- 附:crush 配置体系已重写为 Bash 式 `crushrc`(`mcp add <name> --type stdio|sse|http --command/--args/--env/--url/--header/--timeout/--oauth…`,§8.7 快照过期)[I12];gemini 另有 `.gemini/mcp_config.json` 标准形状入口,且 stdio 子进程默认对宿主环境做凭证类变量脱敏(`*TOKEN*/*SECRET*/*PASSWORD*/*KEY*/*AUTH*` 模式,显式写入 env 视为知情同意)——脱敏这点值得抄。[I11]

### 10.4 `.agents/mcp.json` 算不算标准?

- **没有治理规范**,是事实约定:ZCode 当同 scope 兜底(同 scope `.zcode` 有任何 server 则整个忽略、不合并,未知键整个 server 丢弃)[I6][I7],pi-mcp-adapter 把 `~/.agents/mcp.json` 列为 tool-agnostic 路径[I14]。低成本兼容加分项,别当规范实现。
- 真正有规范背书的是 **Agent Plugins 1.0.0**(agent-plugins.org,Amazon/Cursor/Microsoft/OpenAI/Vercel 共治):插件根 `mcp.json` closed schema(顶层仅 `$schema` + `mcpServers`),transport stdio(`command/args/env/cwd`)/streamable-http(`url/headers` 字面量、禁带凭证)/sse(弃、可选),占位符仅 `${PLUGIN_ROOT}`/`${PLUGIN_DATA}`,1.0.0 不定义 OAuth——管插件打包,不管用户配置文件。[I13]
- `~/.config/mcp/mcp.json`:pi-mcp-adapter 主推的 XDG 风格用户全局路径,目前一家。[I14]

### 10.5 互通最小字段集(Holt 兼容别人配置要解析什么)

必解析/归一:

| 字段 | 出现于 | Holt 导入处理 |
| --- | --- | --- |
| server 名 | 全部(map key) | 清洗 `[A-Za-z0-9_-]`,冲突加后缀 |
| `command` | 全部 stdio(**opencode 是数组**) | 数组拆分:首元素 → command,其余前置 args |
| `args` / `cwd` | 全部 / gemini·codex·pi·Agent Plugins | 透传 / 缺省项目根 |
| `env` | 全部(opencode 叫 `environment`) | 改名归一;值内做变量展开 |
| `url` + `type` | 全部 remote | 归一 `local`→stdio、`remote`/`streamable-http`→http、`sse`→兼容或拒绝、`ws`→警告忽略;kimi「url 即 http」也要收 |
| `headers` | 全部(codex 叫 `http_headers`) | 改名归一;值内展开 |
| `enabled` | codex/opencode/pi(zcode 两种拼写都收) | 归一开关 |
| `includeTools`/`enabled_tools`、`excludeTools`/`disabled_tools` | gemini·pi / codex·crush | 归一白/黑名单(deny 优先) |
| `timeout` | 各家(**单位与语义全不同**:claude per-call ms、gemini request ms 默认 600000、opencode ms 默认 5000、zcode `timeoutMs` 默认 30000、codex 秒 30/300、crush startup 秒) | 按来源语义换算 + clamp,缺省用 Holt 默认,不要盲拷数值 |
| `${VAR}`/`${VAR:-default}` | claude/zcode(插件)/pi | **必须实现**;未设无默认 → 保留字面量 + 警告(Claude 语义) |
| `trust` | gemini | 映射「免审批」提示,缺省 false |

可忽略不报错(导入日志列一遍):Claude 的 `headersHelper`(**值为任意 shell 命令,丢弃 + 显著警告**)、`alwaysLoad`、`oauth`、ws 条目;Codex 的 `auth`/`oauth*`/`scopes`/`required`/`supports_parallel_tool_calls`/`omit_tools_from`/`default_tools_approval_mode`/`tools{}`/`env_vars`/`environment_id`/`name`、`bearer_token`(env var 名可保留,值不内联);gemini 的 Google 专有 `authProviderType`/`targetAudience`/`oauth{…}`;通用未知键一律宽容忽略——ZCode 的严格丢弃是反面教材(用户静默丢 server)。[I3][I7]

差异速查(导入器测试矩阵):

| 维度 | Claude Code | Codex | opencode | kimi | gemini-cli | crush | ZCode | Continue |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 包装键 | `mcpServers` | TOML `mcp_servers` | `mcp` | `mcpServers` | `mcpServers` | Bash `mcp add`/legacy `mcp` | `mcp.servers`/`mcpServers` | 列表 YAML + JSON 直读 |
| command 形态 | string | string | **数组** | string | string | string | string | string |
| env / headers 键名 | `env`/`headers` | `env`/`http_headers` | `environment`/`headers` | `env`/`headers` | `env`/`headers` | `env`/`--header` | `env`/`headers` | yaml |
| remote 判别 | `type`+`url`(无 type 跳过) | 有 `url` | `type:"remote"` | **url 即 http** | `url`=SSE / `httpUrl`=HTTP | `--type` | `type`/推断 | `type` |
| env 展开 | `${VAR}`,`${VAR:-d}` | 无 | `{env:VAR}` | 无 | `$VAR`/`${VAR}`/`%VAR%`(仅 env) | shell 展开(legacy) | 仅插件 | `${{ secrets.X }}` |
| 工具过滤 | 无(权限另做) | enabled/disabled_tools | 无 | 无 | include/excludeTools | enabled/disabled-tools | 无 | 无 |
| 超时语义 | per-call ms | startup/call 秒 | 取工具 ms | 另设 | request ms | startup 秒 | timeoutMs | 无 |

### 10.6 旧快照过期项汇总

| 旧条目 | 现状(2026-09-21) | 来源 |
| --- | --- | --- |
| §3.2 Codex 默认 10s/60s | 30s/300s | [I5] |
| §3.2 Codex 字段集 | + EMA/auth 三值/`startup_timeout_ms`/`omit_tools_from`/`environment_id` 等 | [I3][I4] |
| §8.7 crush 配置在 `crush.json` 的 `mcp` 键 | Bash `crushrc` 为正朔,JSON deprecated 不再收新特性 | [I12] |
| §7.1 pi 生态无 MCP | 出现第三方 `pi-mcp-adapter`(lazy + 代理工具路线),非核心;pi 本体仍无 | [I14] |
| §7.2 pi-core-rs 无 MCP | 仍成立(本机 cargo checkout 复核;唯一 grep 命中是 OAuth scope 字符串) | [I15] |
| 「启示」第 3 条 `mcp.servers` 嵌套形状建议 | 与 ZCode 原生形状撞名不撞语义;若同时收 `.agents/mcp.json` 兜底,避免 ZCode 的「同 scope 不合并」坑 | [I6][I7] |

### 10.7 推荐导入策略

1. **显式导入器,不做环境静默扫描**:UI 入口 → 扫描候选来源 → 列表勾选(带来源/transport/完整 command+env 预览)→ 写入 Holt 原生配置,**永不回写源文件**。参照 ZCode [I6] 与 pi-mcp-adapter(host 发现默认 off)[I14]。
2. **候选来源(一期)**:项目 `.mcp.json`;`~/.claude.json`(顶层 + `projects[*].mcpServers`,不是 settings.json);`~/.codex/config.toml` 的 `[mcp_servers.*]`;`~/.agents/mcp.json` 与 `~/.config/mcp/mcp.json`;`~/.kimi/mcp.json`;`.continue/mcpServers/*.json`。opencode/crush/gemini 形状偏差大、单机收益低,二期。[I1][I3][I6][I8][I10][I14]
3. **解析层**按 §10.5 归一;未知字段宽容忽略并出导入报告;「值为 shell 命令」的字段(`headersHelper`/`http_headers_helper`)丢弃 + 警告;不内联任何凭证。
4. **展开层**:实现 `${VAR}`/`${VAR:-default}`(Claude 语义);转换期顺带处理 gemini 的 `$VAR` 与 opencode 的 `{env:VAR}`;Holt 原生配置存展开前形态,展开在运行时做。
5. **信任边界**:project 级文件导入即视为确认(用户已在对话框看到完整命令);组织/远端下发来源不跟随导入。
6. **超时按语义换算并 clamp**,别把 gemini 的 600000ms 或 codex 的 30s/300s 当 Holt 默认。
7. **二期**:Agent Plugins `mcp.json` 作为插件/分发格式(closed schema、有 JSON Schema 可校验)[I13];`.agents/mcp.json` 作可选读取兜底,但要做合并或明确优先级,别学 ZCode 的不合并。[I6][I7]

## 11. 对 Holt 的启示

1. **接入点:engine 层,产出 `AgentTool`。** pi-core-rs 的 `AgentTool`(name/description/JSON schema/execute 闭包)正是各家把 MCP tool 降维后的统一形态;在 `crates/engine` 内新建 MCP client manager(候选:官方 `rmcp`,Codex/Goose/Amazon Q 同款),把每个 `tools/list` 结果包装成 `AgentTool`,`execute` 里转发 `tools/call`。UI 不需要知道 MCP 的存在,继续只渲染 `MessagePart`——符合 ARCHITECTURE 的 UI/RPC 分层。注意 RPC 合同需同步扩展(server 列表/状态/审批事件),否则 UI 无法呈现"待审批的 MCP 调用"。
2. **命名:从第一天就带前缀。** Codex 与 Kimi 的裸名方案简单,但把冲突处理、权限规则、可读性都变难;Claude Code 的 `mcp__server__tool` 已成行业共识(ZCode 沿用、Claude 系工具互相兼容),且使权限规则可以 `mcp__server` / `mcp__server__tool` 两级表达。
3. **配置:兼容 `mcpServers` 形状。** Claude 的 JSON 形状是事实标准(ZCode 的 `.agents/mcp.json` 兜底、Continue 的 JSON 直读、Amazon Q 的 mirror 注释都是证据)。Holt 的数据目录在 `~/.holt`,建议 `~/.holt/config` 内 `mcp.servers`(嵌套、带 `enabled`/`timeoutMs`/`headers`),文件解析保持严格(未知键报错而非忽略,ZCode 的静默丢弃坑过很多用户)。导入器的字段归一、各家差异速查与导入策略见 §10。
4. **transport:stdio 起步 + Streamable HTTP,直接跳过 SSE。** spec 已完成向 Streamable HTTP 的过渡,SSE 在两家头部实现里一个已删一个标弃;rmcp 的 `transport-child-process` + `transport-streamable-http-client` features 即可覆盖,不必自写 transport。
5. **审批与信任:project 级 server 先确认再连。** Claude Code(project scope 首用弹窗)与 ZCode(自动连接 + Settings 审计)代表两个极端;中间态是合理的——user scope 自动连,workspace scope 首次需确认,同时记住 Claude 的教训:`claude -p` 类无界面会话不能卡在确认上。
6. **资源控制要早做:超时、输出上限、目录上限。** Codex(tool_timeout 60s、每 server 2048 工具/100 页分页、per-tool output_token_limit)、Claude(25k token 输出上限、2KB description 截断、超时自动后台化)都把"防 context 洪水"做成了硬机制,这与 pi 作者对 MCP 上下文成本的核心批评一致 [S36]。Holt 的 RPC 合同本来就对 MCP 载荷有防护注释(crates/engine/src/agent.rs),沿这个方向把上限做进 contract。
7. **lazy loading 可以后置,但接口要预留。** Claude 的 Tool Search 依赖模型侧 `tool_reference`,短期不可复制;更务实的路径是 Codex 式 catalog 缓存 + `enabled_tools/disabled_tools` 白名单,以及 server instructions 注入系统提示(工具多时按需列名)。`listChanged` 通知和 `tools/list` 分页从第一天就处理,避免之后改协议层。
8. **OAuth 可以分两期。** 第一期:stdio + 静态 `headers`/`bearer_token_env_var`(覆盖大多数本地 server);第二期:RFC 8707 + protected resource metadata 的授权码流(参考 Codex `codex mcp login` 与 opencode 的 DCR 实现,回调端口要可固定以兼容预注册 URI)。
9. **反向 server 不是必需品。** Claude Code / Codex 提供该模式主要服务于「把 agent 嵌进其他 harness」;Holt 的对应形态更接近通过既有 `RpcService` 暴露,没有外部消费者需求前可不做。
10. **保持「无 MCP 也能用」。** pi 的立场值得借鉴:内置工具(shell/编辑/检索)+ skills 承担 80% 工作,MCP 是扩展通道而非核心依赖——aider 的完全拒绝和 Zed 式全量接入都各有代价。

## 12. 一手资料索引

### MCP 规范

- [S1]: https://modelcontextprotocol.io/specification/latest (架构与角色;latest 为 2026-07-28)
- [S2]: https://modelcontextprotocol.io/specification/latest (extensions:Tasks / MCP Apps / Skills over MCP)
- [S3]: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports (stdio、Streamable HTTP、HTTP+SSE 兼容、会话/版本头)
- [S4]: https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- [S5]: https://modelcontextprotocol.io/specification/2025-06-18/changelog (OAuth resource server、RFC 8707、elicitation 等)
- [S6]: https://github.com/openai/codex (主仓库)
- [S7]: https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/Cargo.toml
- [S8]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (RawMcpServerConfig / AppToolApproval / McpServerAuth / transport 枚举)
- [S9]: https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs (mcp_servers、oauth 全局项、宽限期)
- [S10]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (McpServerTransportConfig:Stdio/StreamableHttp)
- [S11]: https://github.com/openai/codex/blob/main/codex-rs/tools/src/mcp_tool.rs (裸名、properties 补全、1000B 截断);https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/catalog.rs (同名冲突)
- [S12]: https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/pagination.rs (100 页/2048 项)
- [S13]: https://github.com/openai/codex/blob/main/codex-rs/cli/src/mcp_cmd.rs (list/get/add/remove/login/logout)
- [S14]: https://learn.chatgpt.com/docs/mcp-server (codex mcp-server 弃用公告;codex/codex-reply 工具)
- [S15]: https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs (Subcommand:无 mcp-server,有 app-server)

### Claude Code

- [S16]: https://code.claude.com/docs/en/mcp (安装/transport/超时 tips/资源/roots)
- [S17]: https://code.claude.com/docs/en/mcp (scopes、优先级、${VAR} 展开)
- [S18]: https://code.claude.com/docs/en/mcp (插件 MCP、`mcp__plugin_*` 命名)
- [S19]: https://code.claude.com/docs/en/permissions (mcp__ 规则与 glob 限制)
- [S20]: https://code.claude.com/docs/en/mcp (anthropic/requiresUserInteraction)
- [S21]: https://code.claude.com/docs/en/mcp (Tool Search、ENABLE_TOOL_SEARCH、alwaysLoad)
- [S22]: https://code.claude.com/docs/en/mcp (MCP_TIMEOUT/MCP_TOOL_TIMEOUT/idle timeout/自动后台)
- [S23]: https://code.claude.com/docs/en/mcp (输出上限、maxResultSizeChars)
- [S24]: https://code.claude.com/docs/en/mcp (v1/v2 runtime、list_changed、重连)
- [S25]: https://code.claude.com/docs/en/mcp (prompts→命令、resources→@、elicitation)
- [S26]: https://code.claude.com/docs/en/mcp (claude mcp serve)
- [S27]: https://code.claude.com/docs/en/agent-sdk/mcp (SDK mcpServers/allowedTools/SDK 内嵌 server)

### Kimi CLI

- [S28]: https://github.com/MoonshotAI/kimi-cli
- [S29]: https://github.com/MoonshotAI/kimi-cli/blob/main/src/kimi_cli/soul/toolset.py (MCPTool 裸名/审批/超时);https://github.com/MoonshotAI/kimi-cli/blob/main/pyproject.toml (fastmcp==3.2.4)
- [S30]: https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/mcp.md (kimi mcp 子命令、~/.kimi/mcp.json、OAuth 存储)

### ZCode

- [S31]: 本机 `/Users/onion/.zcode/cli/plugins/cache/zcode-plugins-official/zcode-guide/0.1.0/skills/diagnosing-mcp/SKILL.md`(一手:配置位置/覆盖序/schema/超时/严格校验)
- [S32]: https://zcode.z.ai/en/docs/mcp-services (官方文档:transport、OAuth、导入、信任)
- [S33]: 本机 `/Users/onion/.zcode/cli/plugins/cache/zcode-plugins-official/computer-use/0.5.13/.zcode-plugin/plugin.json`(插件 mcpServers 实例)
- [S34]: 本机 `/Users/onion/.zcode/cli/log/zcode-2026-09-09.jsonl`、`zcode-2026-09-10.jsonl`(mcp.startup.completed、mcp.tools.registered、mcp.pool.*、mcpIsolation、`mcp__langchain_docs__*` 命名实例)

### pi / pi-core-rs

- [S35]: https://github.com/earendil-works/pi (原 badlogic/pi-mono;树内无 mcp 文件)
- [S36]: https://mariozechner.at/posts/2025-11-02-what-if-you-dont-need-mcp/ (作者立场与 token 成本数据)
- [S37]: https://github.com/Onion-L/pi-core-rs (README 分层;src/agent/types.rs 的 AgentTool 定义;无 MCP 模块)

### 其他项目

- [S38]: https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md
- [S39]: https://github.com/QwenLM/qwen-code (packages/core/src/tools/mcp-*.ts)
- [S40]: https://github.com/cline/cline (apps/vscode/src/services/mcp/McpHub.ts、constants.ts、apps/vscode/package.json)
- [S41]: https://github.com/RooCodeInc/Roo-Code/blob/main/src/services/mcp/McpHub.ts (alwaysAllow schema)
- [S42]: https://github.com/aaif-goose/goose (documentation/docs/getting-started/using-extensions.md、guides/managing-tools/goose-permissions.md、crates/goose/Cargo.toml、crates/goose-cli/src/cli.rs、crates/goose-mcp/src/mcp_server_runner.rs)
- [S43]: https://github.com/anomalyco/opencode (packages/opencode/src/mcp/index.ts、packages/core/src/v1/config/mcp.ts)
- [S44]: https://github.com/charmbracelet/crush (README.md MCP 节、go.mod、internal/agent/tools/mcp/channel.go)
- [S45]: https://github.com/All-Hands-AI/OpenHands (specs/mcp-settings.md、src/routes/mcp-settings.tsx)
- [S46]: https://github.com/Aider-AI/aider/issues/2525 、https://github.com/Aider-AI/aider/issues/3314
- [S47]: https://github.com/continuedev/continue/blob/main/docs/customize/deep-dives/mcp.mdx
- [S48]: https://github.com/aws/amazon-q-developer-cli (crates/agent/src/agent/mcp/service.rs、crates/chat-cli/src/cli/agent/mcp_config.rs、crates/chat-cli/src/cli/agent/mod.rs)

### Zed(2026-09-21,源码锚定 main@85b8198e)

- [Z1]: https://zed.dev/docs/ai/mcp (MCP 文档页;仓库源 docs/src/ai/mcp.md)
- [Z2]: https://github.com/zed-industries/zed/blob/main/crates/settings_content/src/project.rs (context_servers / context_server_timeout schema)
- [Z3]: https://github.com/zed-industries/zed/blob/main/crates/project/src/context_server_store.rs (store、状态机、生命周期、OAuth 状态流转)
- [Z4]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/types.rs (手写 MCP 类型)
- [Z5]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/transport.rs 及 transport/stdio_transport.rs、transport/http.rs
- [Z6]: https://github.com/zed-industries/zed/blob/main/crates/context_server/Cargo.toml (无 MCP SDK 依赖)
- [Z7]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/client.rs (JSON-RPC client,默认 60s 超时)
- [Z8]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/protocol.rs (协议版本协商)
- [Z9]: https://github.com/zed-industries/zed/blob/main/crates/context_server/src/oauth.rs (PKCE/DCR/CIMD/keychain)
- [Z10]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/tools/context_server_registry.rs (工具注册、mcp_tool_id、list_changed)
- [Z11]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/thread.rs (工具名清洗/冲突前缀/64 上限/turn 快照)
- [Z12]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/thread.rs (authorize_third_party_tool 审批)
- [Z13]: https://github.com/zed-industries/zed/blob/main/crates/settings_content/src/agent.rs (tool_permissions schema,MCP 不支持 pattern)
- [Z14]: https://zed.dev/docs/ai/tool-permissions
- [Z15]: https://github.com/zed-industries/zed/blob/main/assets/settings/default.json (默认 profile 的 context server 开关)
- [Z16]: https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs (newSession 转发 MCP servers)
- [Z17]: https://zed.dev/docs/ai/external-agents (Native MCP config 归属 external agent)
- [Z18]: https://zed.dev/docs/extensions/mcp-extensions (扩展弃用计划,issue #59351)
- [Z19]: https://github.com/zed-industries/zed/blob/main/crates/settings_ui/src/pages/mcp_servers_page.rs (设置页 UI)
- [Z20]: https://github.com/zed-industries/zed/blob/main/crates/migrator/src/migrations/m_2026_02_04/settings.rs (always_allow_tool_actions → tool_permissions.default)
- [Z21]: https://github.com/zed-industries/zed/blob/main/crates/agent/src/native_agent_server.rs (Zed Agent 作 ACP agent server)
- [Z22]: https://zed.dev/docs/extensions/agent-servers (ACP 扩展弃用)

### 配置互通(2026-09-21)

- [I1]: https://code.claude.com/docs/en/mcp (Claude Code MCP reference 全文:scope/字段/变量展开/导入能力)
- [I2]: https://code.claude.com/docs/en/settings (验证 settings.json 无 mcpServers 键)
- [I3]: https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs (RawMcpServerConfig 当前全集)
- [I4]: https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs (全局键)
- [I5]: https://github.com/openai/codex/blob/main/codex-rs/codex-mcp/src/rmcp_client.rs (DEFAULT_STARTUP_TIMEOUT=30s / DEFAULT_TOOL_TIMEOUT=300s)
- [I6]: https://zcode.z.ai/en/docs/mcp-services (Import From An External Agent / 负载规则)
- [I7]: 本机 `~/.zcode/cli/plugins/cache/zcode-plugins-official/zcode-guide/0.2.0/skills/diagnosing-mcp/SKILL.md` 等一手 guide(2026-09-21)
- [I8]: https://github.com/continuedev/continue/blob/main/docs/customize/deep-dives/mcp.mdx (目录级直读 Claude 系 JSON)
- [I9]: https://opencode.ai/docs/mcp-servers/ (v1 文档,无导入;`.well-known/opencode` 组织下发)
- [I10]: https://github.com/MoonshotAI/kimi-cli/blob/main/docs/en/customization/mcp.md (--mcp-config-file;url 即 http)
- [I11]: https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md (url/httpUrl 判别、凭证脱敏)
- [I12]: https://github.com/charmbracelet/crush/blob/main/docs/config/README.md (Bash crushrc / Legacy JSON)
- [I13]: https://agent-plugins.org/plugin-authors/mcp-servers 及规范仓库 https://github.com/agentplugins/agent-plugins-spec (Agent Plugins 1.0.0 mcp.json closed schema)
- [I14]: https://github.com/nicobailon/pi-mcp-adapter (包页 https://pi.dev/packages/pi-mcp-adapter;/mcp setup、host 发现默认 off)
- [I15]: 本机 cargo checkout `~/.cargo/git/checkouts/pi-core-rs-16822f4cb4a15bb1/`(c6ac601 / e4150c0)全树无 MCP 模块;Holt 根 Cargo.toml git 依赖

### Star 数与仓库状态备注

各仓库 star 数(codex 122,794、pi 103,437、opencode 206,117、gemini-cli 106,878、cline 67,728、OpenHands 87,055、goose 54,060、aider 48,853、continue 35,844、crush 27,972、qwen-code 27,723、Roo-Code 24,305、kimi-cli 11,335、amazon-q-developer-cli 1,983)于 2026-09-10 经 GitHub API 查询;opencode(sst→anomalyco)、goose(block→aaif-goose)、pi(badlogic/pi-mono→earendil-works/pi)均为 API 重定向后的现址。
