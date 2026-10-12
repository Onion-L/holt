# Coding Agent 的 Web Fetch / Web Search 工具实现调研

> 调研日期:2026-09-10。本文面向工程师,梳理主流 coding agent 如何实现 web fetch(抓取 URL)与 web search(联网搜索)两个模型工具。背景:Holt 计划给引擎加这两个工具;Holt 的 agent loop 来自 pi-core-rs,而 pi 生态刻意不做 web 工具,需要先看业界做法再定设计。五家样本:闭源但文档完备的 Claude Code、源码可查的 Codex / opencode / Kimi Code CLI、以及上游 pi。所有结论尽量给到源码级证据;易变内容(版本号、默认值、行为开关)以查阅日期为准。

## 1. 结论先行

- **光谱两端**:pi 刻意不做任何 web 工具("默认没有 web search/fetch 工具,模型可以用 curl"),把 web 能力推给 skills / extension;Codex 走另一个极端——客户端零 HTTP 抓取,搜索与打开页面全部在服务端完成。中间主流(Claude Code、opencode、Kimi)都是**客户端本地抓取 + HTML→文本/markdown + 输出截断**。
- **search 的后端没有一家自己做爬虫索引**:Claude Code 绑 Anthropic API 的 server-side web_search(后端不可换),Codex 绑 OpenAI 后端(`alpha/search` 端点),opencode 外包给 Exa / Parallel 的 hosted MCP(手写 JSON-RPC 薄客户端),Kimi 走自家平台的私有 `/search` 端点。工具本身都很薄,重资产在后端——对 Holt 的启示是 search 工具应做成**可插拔后端 + 无配置时不出现**。
- **fetch 的两种参数哲学**:只给 `url` 返回全文(opencode、Kimi;模型自己读)vs `url + prompt` 用小模型提炼成答案(Claude Code 独有,"lossy by design",省 context 但失真)。Codex 无 fetch 工具,打开/翻页是 search 后端的命令。
- **HTML→文本转换是每家自选件**:opencode 用 turndown(JS)+ htmlparser2 手写遍历,Kimi 用 trafilatura(Python,唯一接近 readability 正文的),Codex Go 时代前身用 goquery + html-to-markdown;pi 生态(含 pi-core-rs)完全没有现成实现,Rust 侧要自选库。
- **限制是共同惯例**:下载上限(opencode 5MB 双重检查)、输出 50KB 封顶(opencode 通用截断,超出落盘;与 Holt `grep` 的 50KB 封顶同款)、50k 字符(Kimi)、超时 30s 默认/120s 上限(opencode)、180s(Kimi)。把限制写进工具 description 是各家(尤其 pi 系)的一贯做法。
- **权限默认值分两派**:Claude Code 默认逐次 ask,但有细粒度 `WebFetch(domain:...)` 规则与预批准文档域;opencode(`"*": "allow"` 基线)、Kimi(不在审批体系)、Codex("no per-call approval")默认静默放行。fetch 是读取型但外发网络,默认档位是产品决策。
- **安全防护参差,普遍薄弱**:只有 Claude Code 有出站预检(抓取前把 hostname 送 api.anthropic.com 查安全 blocklist);opencode / Kimi 既不读 robots.txt、无域名黑白名单、也无 SSRF/内网防护;UA 策略三派——自报家门(Claude-User)、先伪装 Chrome 被 Cloudflare 识破再自报(opencode)、伪装 Chrome(Kimi)。
- **优雅降级有范式**:Kimi 的 `SkipThisTool`(服务没配,工具直接从清单消失)与 opencode 的系统提示兜底(没有 search 工具时教模型用 webfetch 抓搜索引擎 URL)都值得学;Claude 的"后端不可配,要换就接 MCP"是反面参考(对多 provider 的 Holt 而言)。

## 2. 逐家拆解

### 2.1 Claude Code(闭源;一手=官方文档 + changelog)

**WebFetch** —— 客户端本地工具,参数 `url` + `prompt` [S1]:

- 抓取页面,HTML→Markdown(内部实现,不可配),然后"用一个小而快的模型把 prompt 跑在内容上",Claude 拿到的是提炼答案而非原文——文档明说 "This makes WebFetch lossy by design",要原文用 Bash curl [S1]。
- HTTP 自动升级 HTTPS;大页面在处理前先截断到固定字符上限;跨主机重定向不自动跟随,而是返回重定向信息让 Claude 发第二次调用;529/限流带退避重试 [S1]。
- **15 分钟缓存**,v2.1.233+ 可用 `CLAUDE_CODE_WEBFETCH_CACHE_TTL_MS` 调整(默认 900000ms,启动时读一次)[S1][S3]。
- UA 以 `Claude-User` 开头,Accept 头内容协商优先 Markdown;changelog v2.1.83 的说法是"自报身份便于站点方通过 robots.txt 识别/放行"——**工具本身不读取也不执行 robots.txt** [S1][S6]。
- **出站安全预检**:抓取前把 hostname(不含完整 URL)送到 `api.anthropic.com` 对照安全 blocklist,通过的 hostname 缓存 5 分钟;与模型提供商无关,`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` 关不掉,唯一开关是 settings 键 `skipWebFetchPreflight: true` [S4]。
- 权限:默认逐次 ask(Manual / acceptEdits 模式),例外是已有规则覆盖的域名和内置一组 preapproved 文档域;"Yes, and don't ask again" 把 `WebFetch(domain:...)` allow 规则写入该仓库 `.claude/settings.local.json`。规则语法:`WebFetch(domain:example.com)`、`WebFetch(domain:*.example.com)`(匹配子域不匹配裸域)、`WebFetch(domain:*)`、裸 `WebFetch`;deny 裸名=直接移除工具,deny `domain:*`=保留工具但拒绝每次调用;`domain:` 规则还同步进 sandbox 网络允许/拒绝列表;显式规则优先于 preapproved 集合 [S2]。

**WebSearch** —— 后端是 Anthropic API 的 server-side `web_search`:

- "对 Anthropic 的 web search 后端执行查询,返回结果标题和 URL,**不抓取结果页面**"(读页面需再调 WebFetch);"The search backend is not configurable. To search with a different provider, add an MCP server" [S1]。
- API 层参数:`max_uses`、`allowed_domains`/`blocked_domains`(互斥)、`user_location`;计费 $10/1000 次另加 token [S5]。Claude Code 侧单次调用最多发起 8 次后端搜索;**每会话(主对话+subagent 合计)上限 200 次**(v2.1.212+,`CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION` 可调不可关,`/clear` 重置;到限返回"用已有信息继续"提示而非报错)[S1][S3]。
- 权限同样默认 ask,但规则只有裸 `WebSearch` 一种写法(无域粒度);Amazon Bedrock 不暴露该 server-side 工具;现行文档已无早期的 "US-only" 表述 [S1][S2]。
- v2.1.248 的 `--restricted` 模式会移除 WebFetch(除非 `--tools` 点名)[S6]。

### 2.2 OpenAI Codex(Rust;commit `b348fc266`,2026-09-10)

两条 web 路径,**没有独立的 client-side fetch 工具**;两条都由服务端执行 [S8][S10][S11]:

1. **托管 `web_search`**(Responses API 托管工具):定义在 `tools/src/tool_spec.rs:39-53`,带 `external_web_access`、`indexed_web_access`、`filters`(allowed_domains)、`user_location`、`search_context_size`、`search_content_types`,直接序列化进请求的 `tools` 数组。模型不发起函数调用,API 服务端执行后流式回 `WebSearchCall { id, status, action }`(`protocol/src/models.rs:1178-1192`,action 分 search/open_page/find_in_page),client 只在 `core/src/event_mapping.rs:229-240` 映射成事件,TUI 渲染 "Searching/Searched the web" [S8][S9]。
2. **独立 `web.run` 扩展工具**(新 crate `ext/web-search`):以普通 function tool 暴露,但执行体不做本机抓取——把最近对话片段 + 命令打包 POST 到 OpenAI 后端 `alpha/search`(`codex-api/src/endpoint/search.rs:31-33`;`ext/web-search/src/tool.rs:90-110`),搜索、打开页面、翻页、截图全在服务端完成,`ref_id`(turn0search0 形态)是服务端会话状态。参数 schema 由 `SearchCommands` 经 schemars 生成:`search_query: [{q, recency?, domains?}]`(单次最多 4 条)、`open: [{ref_id|URL, lineno?}]`、`click`、`find` 等;描述文本外置于 `web_run_description.md`(include_str! 引入),写明"何时必须浏览"的决策边界、引用格式、25 词逐字引用上限 [S10][S11]。

两者互斥(standalone 可用时不发托管工具,`core/src/tools/spec_plan.rs:609-616`);standalone 还需 provider 能力声明 + 默认关闭的 feature flag(`spec_plan.rs:1038-1047`)[S12]。

**配置与权限** [S13][S14][S15]:

- config.toml 顶层 `web_search = "disabled"|"cached"|"indexed"|"live"`(默认 **Cached**:注入工具但 `external_web_access=false`,只读缓存不联网);`tools.web_search = { context_size, allowed_domains, location }`;`--search` flag 即 `web_search="live"`,帮助文本明写 "no per-call approval"。旧名 `web_search_request` 已 deprecated。
- 无逐次审批;硬闸只有两个:provider 能力声明,以及 review 子代理强制 `WebSearchMode::Disabled`(`core/src/tasks/review.rs:107-114`)。

### 2.3 opencode(TS;commit `859106eb`,dev 分支)

**webfetch**(`packages/opencode/src/tool/webfetch.ts`)—— 纯 HTTP 客户端,**没有任何 LLM 提炼步骤**(工具描述里 "Results may be summarized" 是残留文案,历史 tag 也从未有过 summarize 行为)[S16][S20]:

- 参数:`url`、`format`(`"text"|"markdown"|"html"`,默认 markdown)、`timeout`(默认 30s,上限 120s);无 prompt 参数,全文直接返回。描述声称 HTTP 自动升级 HTTPS,代码只校验前缀并无升级逻辑(文档-实现偏差)[S16]。
- 原生 fetch(Effect `FetchHttpClient`);Content-Length 与实际 body 双重检查,**超 5MB 直接抛错**;默认伪装 Chrome 143 UA,收到 403 且 `cf-mitigated: challenge`(Cloudflare bot 挑战)时改用诚实 UA `"opencode"` 重试一次 [S16]。
- HTML→markdown 用 **turndown**(atx 标题、fenced code、剥离 script/style/meta/link);HTML→text 用 htmlparser2 手写遍历跳过 script/style/noscript/iframe 等;无 readability 类正文抽取;非 HTML 原样返回;图片(非 svg)转 base64 data-URL 作为 attachments 返回;不带任何授权头 [S16]。

**websearch**(`websearch.ts` + `mcp-websearch.ts`)—— 内置工具但完全外包 [S17][S18]:

- 对 Exa(`https://mcp.exa.ai/mcp`)和 Parallel(`https://search.parallel.ai/mcp`)两个 hosted MCP 发手写 JSON-RPC `tools/call`(25s 超时,SSE 解析);参数 `query` 必填 + `numResults`(默认 8)、`livecrawl`、`type`、`contextMaxCharacters`(后四项仅 Exa 生效)。
- provider 选择:env/flags 可强制,**都没有时按 sessionID 校验和取模 50/50 随机分给两家**。
- 暴露条件(注册表 `registry.ts:58-65`):仅当 provider 是 opencode 自家网关,或设了 `OPENCODE_ENABLE_EXA` / `OPENCODE_ENABLE_PARALLEL`;其他 provider 不暴露,系统提示的兜底是教模型"用 webfetch 抓 `https://www.google.com/search?q=…`"(`src/session/prompt/beast.txt:68`)[S18][S19]。

**权限与截断** [S19]:

- 两工具执行前都过 `ctx.ask`:permission 分别为 `webfetch`(按 URL)/ `websearch`(按 query),默认 agents 基线 `"*": "allow"` 静默放行;用户可按通配符 allow/ask/deny,deny `*` 时工具直接从请求中剔除;ask 批准 "always" 写入 `["*"]` 本会话全放行;`explore` 子代理显式白名单这两个工具、其余全 deny。
- 输出统一截断:超 2000 行或 50KB 时保留头部预览,全文落盘 `<data>/tool-output/` 并提示模型用 Grep/Read 处理(`tool.ts:131-145`、`truncate.ts:14-15`)。

历史对照:Go 前身(v0.0.1,`internal/llm/tools/fetch.go`)用 goquery + JohannesKaufmann/html-to-markdown,结构与现代一致——**webfetch 一直是纯抓取,fetch+summarize 从未存在过** [S21]。

### 2.4 Kimi Code CLI(Python;commit `86f1364`,v1.50.0)

工具仅两个:`SearchWeb`(`src/kimi_cli/tools/web/search.py`)、`FetchURL`(`fetch.py`),主 agent 与全部子 agent 默认携带 [S22][S23]:

- **SearchWeb**:后端是 Moonshot 自建搜索服务——POST `https://api.kimi.com/coding/v1/search`(可被 `KIMI_CODE_BASE_URL` 覆盖),body `{text_query, limit, enable_page_crawling, timeout_seconds: 30}`,凭据复用登录态(API key 或 OAuth token),客户端超时 total=180s。`include_content=True` 映射为服务端 `enable_page_crawling`(让服务端爬回页面全文)。响应 `search_results[]` 渲染为 Title/Date/URL/Summary 文本块。**服务未配置时构造器抛 `SkipThisTool`,工具静默从模型清单消失**(`soul/toolset.py:694-699` 捕获后 continue)[S23][S24]。
- **FetchURL** 双路径:优先走 Moonshot Fetch 服务(POST `/coding/v1/fetch`,`Accept: text/markdown`),失败回退本地 aiohttp + **trafilatura** 提正文(`include_comments/tables=True`,输出纯文本非 markdown);UA 伪装 Chrome 91,总超时 180s,`text/plain|markdown` 原样透传,提取失败返回"页面可能需要 JavaScript 渲染"。结果 **50,000 字符**硬截断加 `[...truncated]` 标记 [S23]。
- 参数:`SearchWeb(query, limit 1–20 默认 5, include_content 默认 False)`(描述引导模型改写 query 而非调大 limit);`FetchURL(url)` 仅此一个 [S23]。
- **防护为零**:无 robots.txt、无域名黑白名单、无 SSRF 防护、无重定向限制,HTTP 状态码是唯一闸门;**两个工具都不在审批体系内**,默认免确认直接执行(对比 Shell/写文件需审批)[S23]。
- 配置:`[services.moonshot_search]` / `[services.moonshot_fetch]`(base_url、api_key、oauth),`/login` OAuth 或 `/setup` API key 自动写入;官方文档明说这两个工具目前只有 Kimi Code 平台提供,其他 provider 下 SearchWeb 不可用、FetchURL 回退本地抓取 [S24][S25]。
- 解耦事实:Kimi 开放平台的 K2 系列在 API 层有 `$web_search` builtin_function(服务端执行、按次计费),但 kimi-cli 有意不用,改走平台 sidecar 端点——agent 逻辑与模型供应商解耦 [S26]。

### 2.5 pi / pi-core-rs(刻意不做)

- 上游 TS 版(earendil-works/pi,原 badlogic/pi-mono;commit `bbb61e34`,2026-09-10):内置工具只有 bash/edit/find/grep/ls/powershell/read/write,**没有任何 web fetch/search/browser 工具,也没有官方扩展示例**。README 明言默认四件套 read/write/edit/bash;博客原话:"By default, pi has no web search or fetch tool. However, it can use curl or read files from disk." 哲学是"最小核心,其余用 extension/skill/第三方包搭",且坚决不做 MCP [S27][S28]。
- web 能力的官方出口是 **skills**:`docs/skills.md:188-231` 给了完整的 `brave-search` skill 示例(bash 脚本 + Brave Search API),并指向官方 skill 仓库 pi-skills("Web search, browser automation, …")。自定义工具的扩展点是 `pi.registerTool({ name, label, description, parameters, execute })` [S29][S30]。
- Rust 版 pi-core-rs(commit `c6ac601`,v0.84.4):只移植了 read/write/edit/bash(含图片处理),连 grep/find/ls 都没有;`src/agent/search/` 是会话历史搜索,与 web 无关;reqwest 依赖只服务于 provider 传输层 [S31]。
- **工具扩展接口**(对接入方最关键):无 trait,是数据 + 闭包——低层 `AgentTool { name, label, description, parameters: JSON schema, execute: Arc<dyn Fn(…)->BoxFuture<…>> }`(`src/agent/types.rs:300-317`);高层 `AgentHarnessTool` 多一个 opaque context(内置工具 downcast 出 `ExecutionToolContext { env }`)。**新增 web 工具不需要 ExecutionEnv**,直接构造 `AgentTool` 即可;pi-core-rs 自带可参考的 HTTP 底座(`src/ai/utils/http.rs` 的 `HttpFetch` trait、带取消的 `HttpRequest`、代理感知的 `ReqwestFetch`),但只用于 provider 层 [S31][S32]。
- 工具描述里写明截断/大小限制是 pi 工具设计的一贯做法(read 工具、官方 rg 示例都如此)[S31]。

## 3. 横向对比

| 维度 | Claude Code | Codex | opencode | Kimi CLI | pi / pi-core-rs |
| --- | --- | --- | --- | --- | --- |
| fetch 工具 | WebFetch(本地抓) | 无(open 命令在服务端;curl 兜底) | webfetch(本地抓) | FetchURL(服务端优先+本地回退) | 无(bash+curl) |
| fetch 提炼 | 小模型按 prompt 提炼,有损 | — | 无,全文 | 无,全文 | — |
| HTML 处理 | 内部 HTML→MD | — | turndown / htmlparser2 | trafilatura→纯文本 | — |
| search 工具 | WebSearch | 托管 `web_search` + `web.run` | websearch(条件暴露) | SearchWeb(未配置即消失) | 无 |
| search 后端 | Anthropic API server-side(不可换) | OpenAI 服务端 | Exa/Parallel hosted MCP | Kimi Code 平台私有端点 | — |
| search 是否读页面 | 否(配 WebFetch 用) | 是(open/click) | 否(配 webfetch 用) | 可选(include_content) | — |
| 下载/输出限制 | 大页面先截断再处理 | 服务端 output 预算 | 5MB 硬限;输出 2000 行/50KB 落盘 | 输出 50k 字符截断 | — |
| 缓存 | 15 分钟(TTL 可调) | — | 无 | 服务端 | — |
| robots.txt | 不执行(UA 自报家门) | — | 无 | 无 | — |
| SSRF/域名防护 | 出站 blocklist 预检 + domain 规则 | 服务端承担 | 无 | 无 | — |
| UA 策略 | `Claude-User` | — | Chrome 143→403 时自报 | Chrome 91 | — |
| 权限默认 | ask(域规则/preapproved 可放行) | 无逐次审批 | allow(可按 URL/query 规则收敛) | 免审批 | — |
| 未配置时 | 工具仍在 | Cached 模式默认在场 | 条件注册 | SkipThisTool 消失 | — |

## 4. 对 Holt 的落地建议

结合五家做法与 Holt 现状(agent loop 来自 pi-core-rs;工具组装在 `crates/engine/src/tools.rs:615` 的 `execution_tools_for_model`;自研 `grep` 工具是现成的 holt 原生 `AgentTool` 模板;ADR-0014 权限门已覆盖 write/edit/bash;workspace 已有 reqwest 0.12):

1. **实现层次:holt 原生 `AgentTool`,pi-core-rs 零改动。** 仿照 `tools/grep.rs`(DESCRIPTION 写明限制、`Deserialize` 输入 struct、50KB 输出封顶、取消 token 贯穿),新增 `tools/web_fetch.rs` 与 `tools/web_search.rs`,在 `execution_tools_for_model` 追加即可。pi-core-rs 的 `AgentTool` 就是数据+闭包,不需要 ExecutionEnv。
2. **fetch 走"全文返回"路线(opencode/Kimi),第一版不做小模型提炼。** 参数 `url`(+ 可选 `format: text|markdown`,默认 markdown);理由:提炼要多一次 provider 往返且有损,Holt 的 grep/read 风格一直是"把限制写清楚、模型自己读";UI 渲染 `MessagePart` 时全文截断也更可预期。HTML→markdown 在 Rust 侧需新引库(htmd、readability 类候选,**未验证,需 PoC**);纯文本回退可学 opencode 的 html5ever 手写遍历思路。
3. **fetch 的硬限制对齐现有惯例**:输出 50KB 封顶(与 `grep.rs` 的 `OUTPUT_BYTE_CAP` 一致)、下载上限约 5MB(opencode)、超时 30s 默认/上限可调、全程走 run 的取消 token(grep 已示范 spawn_blocking + select 的写法,fetch 是纯异步更简单)。UA 自报家门(`holt/<version>`),只允许 http(s);**建议补上业界普遍缺失的 SSRF 防护**(拒绝环回/内网/链路本地地址),这是低成本的差异化。robots.txt 各家都不执行,不建议第一版做。
4. **search 做成可插拔后端,未配置时工具不出现。** 定义一个内部搜索后端 trait(Brave/Exa/Tavily 之类的 REST API,或学 opencode 直接打 hosted MCP——薄客户端即可);参照 Kimi 的 `SkipThisTool` 语义:无后端配置 → 不注册工具,而不是注册后报错。**不要**绑死某个模型 provider 的 server-side search(holt 是多 provider 的,Claude Code "后端不可配"是被验证过的反面模式)。输出 title/url/snippet 列表,读页面交给 fetch——search 与 fetch 职责分离是 Claude Code/opencode 的共同分工。
5. **权限挂 ADR-0014 门。** fetch 是读取型但外发网络,search 同理;建议默认不拦(与 grep 同级)、提供 settings 级域名 deny 规则的接口(学 Claude 的 `domain:` 语法、opencode 的通配符匹配),PermissionMode 收敛行为可以后置迭代。**先想清楚默认档再上线**:四家里三家默认放行,只有 Claude Code 默认逐次 ask。
6. **无 search 后端时的兜底可选**:学 opencode 在系统提示里教模型"用 webfetch 抓搜索引擎 URL";另外 pi 的提醒仍然成立——bash+curl 本来就是万能兜底,专用工具的增量价值在结构化输出、权限可控、不依赖用户机器有 curl,工具描述里应把这三点价值写清楚。

## 5. 引用来源

### Claude Code

- [S1]: https://code.claude.com/docs/en/tools-reference (WebFetch/WebSearch 行为、缓存、限额、重定向)
- [S2]: https://code.claude.com/docs/en/permissions (domain: 规则语法、默认审批、preapproved)
- [S3]: https://code.claude.com/docs/en/env-vars (CLAUDE_CODE_WEBFETCH_CACHE_TTL_MS、CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION)
- [S4]: https://code.claude.com/docs/en/data-usage#webfetch-domain-safety-check (hostname blocklist 预检、skipWebFetchPreflight)
- [S5]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool (API server-side 参数与计费)
- [S6]: https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md (v2.1.83 Claude-User UA、v2.1.248 --restricted、v2.1.212 限额)

### Codex(commit `b348fc26674189f758d5941cdab3f78f258b2aa7`,路径相对 codex-rs/)

- [S7]: https://github.com/openai/codex
- [S8]: tools/src/tool_spec.rs:39-53(ToolSpec::WebSearch 字段)
- [S9]: protocol/src/models.rs:1178-1192(WebSearchCall);core/src/event_mapping.rs:229-240;tui/src/history_cell/search.rs:8-12
- [S10]: ext/web-search/src/tool.rs:40-110;ext/web-search/src/web_run_description.md;ext/web-search/src/schema.rs:6-35
- [S11]: codex-api/src/endpoint/search.rs:31-33(alpha/search);codex-api/src/search.rs:32-78, 298-305
- [S12]: core/src/tools/spec_plan.rs:596-633, 1038-1047, 609-616;core/src/tools/hosted_spec.rs:14-46;features/src/lib.rs:1074-1078
- [S13]: protocol/src/config_types.rs:375-382(默认 Cached);core/src/config/mod.rs:3699-3700;config/src/config_toml.rs:445-446
- [S14]: tui/src/cli.rs:68-70(--search 与 "no per-call approval");cli/src/main.rs:2309-2314
- [S15]: core/src/tasks/review.rs:107-114(review 强制 Disabled)

### opencode(commit `859106eb17d5b840475f5e4b78e64c9622f8750e`,现址 anomalyco/opencode)

- [S16]: packages/opencode/src/tool/webfetch.ts(全文;参数/UA/5MB/turndown/htmlparser2)
- [S17]: packages/opencode/src/tool/websearch.ts;packages/opencode/src/tool/mcp-websearch.ts
- [S18]: packages/opencode/src/tool/registry.ts:58-65, 218-249, 292-295;src/config/runtime-flags.ts:31-40
- [S19]: packages/opencode/src/agent/agent.ts:119-134, 199-212;src/permission/index.ts:28-38, 143-151, 204-214;src/session/tools.ts:81-88;src/session/llm/request.ts:207-213;packages/opencode/src/tool/truncate.ts:14-15, 127-137;src/session/prompt/beast.txt:68
- [S20]: packages/web/src/content/docs/tools.mdx:254-286;docs/permissions.mdx:161-162
- [S21]: tag v0.0.1 internal/llm/tools/fetch.go;tag v0.1.0 packages/opencode/src/tool/webfetch.ts(历史对照)

### Kimi Code CLI(commit `86f136422a0aae6b217ea49e7ea1d2e8a1defcd2`,v1.50.0)

- [S22]: https://github.com/MoonshotAI/kimi-cli;src/kimi_cli/agents/default/agent.yaml(allowed_tools)
- [S23]: src/kimi_cli/tools/web/search.py;src/kimi_cli/tools/web/fetch.py;src/kimi_cli/tools/utils.py:50(50k 截断);src/kimi_cli/soul/toolset.py:694-699(SkipThisTool)
- [S24]: src/kimi_cli/auth/platforms.py:64(api.kimi.com 端点);src/kimi_cli/config.py:128-168;src/kimi_cli/auth/oauth.py:595-608;docs/en/configuration/providers.md:142-153
- [S25]: pyproject.toml(trafilatura==2.0.0、aiohttp==3.13.3)
- [S26]: https://platform.kimi.ai/docs/guide/use-web-search(K2 `$web_search` builtin_function)

### pi / pi-core-rs

- [S27]: https://github.com/earendil-works/pi(原 badlogic/pi-mono;commit `bbb61e34`;packages/coding-agent/src/core/tools/、README.md:91, 495-499)
- [S28]: https://mariozechner.at/posts/2025-11-30-pi-coding-agent/("By default, pi has no web search or fetch tool")
- [S29]: pi 仓库 docs/skills.md:188-232(brave-search skill 示例)
- [S30]: https://github.com/badlogic/pi-skills(官方 web 能力 skill 仓库);packages/coding-agent/examples/extensions/truncated-tool.ts:49(registerTool 示例)
- [S31]: https://github.com/Onion-L/pi-core-rs(commit `c6ac601`,v0.84.4;本机 checkout `~/.cargo/git/checkouts/pi-core-rs-16822f4cb4a15bb1/`;src/agent/types.rs:300-317、src/agent/harness/tools/read.rs:38-67、src/ai/utils/http.rs:114-119、src/ai/utils/reqwest_fetch.rs:16)
- [S32]: pi-core-rs src/agent/harness/types.rs:79-92;src/agent/harness/tools/tool_context.rs:14-17

### Holt 本仓库(落地锚点)

- crates/engine/src/tools.rs:615(`execution_tools_for_model` 工具组装)、:590-605(`with_execution_context`)
- crates/engine/src/tools/grep.rs(holt 原生 AgentTool 模板:DESCRIPTION/输出 50KB/取消)
- crates/engine/src/gate.rs(ADR-0014 权限门:pending Approval、PermissionMode)
- 根 Cargo.toml:84(reqwest 0.12,rustls)
