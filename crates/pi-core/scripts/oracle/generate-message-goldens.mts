/**
 * TypeScript oracle golden generator for pi-ai message serialization.
 *
 * Constructs message/content/event objects shaped by
 * `pi-core/ai/src/types.ts` and serializes them with JSON.stringify, producing
 * the expected wire format fixtures that the Rust port byte-compares.
 *
 * Run:  node --experimental-strip-types scripts/oracle/generate-message-goldens.ts
 * Write: scripts/oracle/generate-message-goldens.ts --write
 */

import type {
  AssistantMessage,
  AssistantMessageEvent,
  AnthropicMessagesCompat,
  BedrockCompat,
  Context,
  DeferredHandle,
  ImageContent,
  Message,
  Model,
  OpenAICompletionsCompat,
  OpenAIResponsesCompat,
  TextContent,
  ThinkingContent,
  Tool,
  ToolCall,
  ToolResultMessage,
  Usage,
  UserMessage,
} from "../../pi-core/ai/src/types.ts";

type AssistantContent = TextContent | ThinkingContent | ToolCall;
type BlockContent = TextContent | ImageContent;
type ModelCompat =
  | OpenAICompletionsCompat
  | OpenAIResponsesCompat
  | AnthropicMessagesCompat
  | BedrockCompat;

const zeroCost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 };

const usage: Usage = {
  input: 120,
  output: 45,
  cacheRead: 2000,
  cacheWrite: 300,
  cacheWrite1h: 100,
  reasoning: 20,
  totalTokens: 2665,
  cost: { input: 0.001, output: 0.002, cacheRead: 0.0001, cacheWrite: 0.0005, total: 0.0036 },
};

const text: TextContent = { type: "text", text: "Hello world" };
const textWithSignature: TextContent = {
  type: "text",
  text: "Signed",
  textSignature: '{"v":1,"id":"msg_01"}',
};
const thinking: ThinkingContent = { type: "thinking", thinking: "Let me reason" };
const thinkingSigned: ThinkingContent = {
  type: "thinking",
  thinking: "Reasoned",
  thinkingSignature: "sig==",
};
const thinkingRedacted: ThinkingContent = {
  type: "thinking",
  thinking: "",
  thinkingSignature: "redacted-payload==",
  redacted: true,
};
const image: ImageContent = { type: "image", data: "aGVsbG8=", mimeType: "image/png" };
const toolCall: ToolCall = {
  type: "toolCall",
  id: "call_1",
  name: "read_file",
  arguments: { path: "/tmp/x", limit: 10, nested: { flag: true, nothing: null, list: [1, "two"] } },
};
const toolCallNamespaced: ToolCall = {
  type: "toolCall",
  id: "call_2",
  name: "search",
  arguments: { query: "q" },
  namespace: "web/search",
};

const assistantContent: AssistantContent[] = [
  thinking,
  thinkingSigned,
  thinkingRedacted,
  text,
  textWithSignature,
  toolCall,
  toolCallNamespaced,
];

const assistantMessage: AssistantMessage = {
  role: "assistant",
  content: assistantContent,
  api: "anthropic-messages",
  provider: "anthropic",
  model: "claude-opus-4-7",
  responseModel: "claude-opus-4-7-20260101",
  responseId: "msg_01ABC",
  usage,
  stopReason: "toolUse",
  rawStopReason: "tool_use",
  endTurn: false,
  timestamp: 1735689600000,
};

const assistantMinimal: AssistantMessage = {
  role: "assistant",
  content: [{ type: "text", text: "Done" }],
  api: "openai-completions",
  provider: "openai",
  model: "gpt-5",
  usage: {
    input: 1,
    output: 2,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 3,
    cost: zeroCost,
  },
  stopReason: "stop",
  timestamp: 1735689600001,
};

const userString: UserMessage = { role: "user", content: "Plain text prompt", timestamp: 1735689600002 };

const userBlocks: UserMessage = {
  role: "user",
  content: [text, image],
  timestamp: 1735689600003,
};

const toolResult: ToolResultMessage = {
  role: "toolResult",
  toolCallId: "call_1",
  toolName: "read_file",
  content: [text, image],
  isError: false,
  timestamp: 1735689600004,
};

const toolResultError: ToolResultMessage = {
  role: "toolResult",
  toolCallId: "call_2",
  toolName: "search",
  content: [{ type: "text", text: "boom" }],
  isError: true,
  timestamp: 1735689600005,
};

const deferredHandle: DeferredHandle = {
  provider: "openai",
  modelId: "o4-mini",
  api: "openai-responses",
  id: "resp_deferred_1",
  expiresAt: 1735693200000,
  pollAfterMs: 1500,
  data: { kind: "row", batch: "b1" },
};

const assistantDeferred: AssistantMessage = {
  role: "assistant",
  content: [],
  api: "openai-responses",
  provider: "openai",
  model: "o4-mini",
  usage: {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 0,
    cost: zeroCost,
  },
  stopReason: "deferred",
  deferred: deferredHandle,
  timestamp: 1735689600006,
};

const messageWithError: AssistantMessage = {
  role: "assistant",
  content: [],
  api: "google-generative-ai",
  provider: "google",
  model: "gemini-3-pro",
  usage: {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 0,
    cost: zeroCost,
  },
  stopReason: "error",
  errorMessage: "503 Service Unavailable",
  timestamp: 1735689600007,
};

const messages: Message[] = [
  userString,
  userBlocks,
  assistantMessage,
  assistantMinimal,
  toolResult,
  toolResultError,
  assistantDeferred,
  messageWithError,
];

const partial: AssistantMessage = {
  role: "assistant",
  content: [{ type: "text", text: "Hel" }],
  api: "anthropic-messages",
  provider: "anthropic",
  model: "claude-opus-4-7",
  usage: {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 0,
    cost: zeroCost,
  },
  stopReason: "pending",
  timestamp: 1735689600008,
};

const events: AssistantMessageEvent[] = [
  { type: "start", partial: { ...partial, content: [] } },
  { type: "text_start", contentIndex: 0, partial: { ...partial, content: [{ type: "text", text: "" }] } },
  {
    type: "text_delta",
    contentIndex: 0,
    delta: "Hel",
    partial: { ...partial, content: [{ type: "text", text: "Hel" }] },
  },
  {
    type: "text_end",
    contentIndex: 0,
    content: "Hello",
    partial: { ...partial, content: [{ type: "text", text: "Hello" }] },
  },
  {
    type: "thinking_start",
    contentIndex: 1,
    partial: { ...partial, content: [{ type: "thinking", thinking: "" }] },
  },
  {
    type: "thinking_delta",
    contentIndex: 1,
    delta: "rea",
    partial: { ...partial, content: [{ type: "thinking", thinking: "rea" }] },
  },
  {
    type: "thinking_end",
    contentIndex: 1,
    content: "reasoning",
    partial: { ...partial, content: [{ type: "thinking", thinking: "reasoning" }] },
  },
  {
    type: "toolcall_start",
    contentIndex: 2,
    partial: { ...partial, content: [{ type: "toolCall", id: "", name: "", arguments: {} }] },
  },
  {
    type: "toolcall_delta",
    contentIndex: 2,
    delta: '{"path"',
    partial: { ...partial, content: [{ type: "toolCall", id: "call_1", name: "read_file", arguments: {} }] },
  },
  {
    type: "toolcall_end",
    contentIndex: 2,
    toolCall,
    partial: { ...partial, content: [toolCall] },
  },
  {
    type: "done",
    reason: "toolUse",
    message: { ...assistantMessage, content: [text, toolCall] },
  },
  {
    type: "error",
    reason: "aborted",
    error: { ...partial, stopReason: "aborted", errorMessage: "Request was aborted" },
  },
];

const model: Model<"anthropic-messages"> = {
  id: "claude-opus-4-7",
  name: "Claude Opus 4.7",
  api: "anthropic-messages",
  provider: "anthropic",
  baseUrl: "https://api.anthropic.com",
  reasoning: true,
  input: ["text", "image"],
  cost: {
    input: 5,
    output: 25,
    cacheRead: 0.5,
    cacheWrite: 6.25,
    tiers: [
      { input: 2.5, output: 12.5, cacheRead: 0.25, cacheWrite: 3.125, inputTokensAbove: 200000 },
    ],
  },
  contextWindow: 200000,
  maxTokens: 64000,
};

const modelCompatSamples: Record<string, ModelCompat> = {
  openaiCompletions: {
    supportsStore: false,
    supportsDeveloperRole: true,
    supportsReasoningEffort: true,
    supportsUsageInStreaming: true,
    supportsFinishReason: true,
    maxTokensField: "max_completion_tokens",
    requiresToolResultName: false,
    requiresAssistantAfterToolResult: false,
    requiresThinkingAsText: true,
    requiresReasoningContentOnAssistantMessages: false,
    thinkingFormat: "openai",
    chatTemplateKwargs: { enable_thinking: { $var: "thinking.enabled", omitWhenOff: true } },
    supportsStrictMode: true,
    cacheControlFormat: "anthropic",
    sendSessionAffinityHeaders: true,
    deferredToolsMode: "kimi",
    sessionAffinityFormat: "openai",
    supportsLongCacheRetention: true,
    thinkingTokenBudgetField: "thinking_budget",
    supportsThinkingTokenBudget: true,
    supportsOpenAIGrammarTools: true,
    zaiToolStream: false,
    openRouterRouting: { order: ["anthropic"], only: ["openai"], allow_fallbacks: false },
    vercelGatewayRouting: { only: ["bedrock"], order: ["anthropic", "openai"] },
  },
  openaiResponses: {
    supportsDeveloperRole: false,
    sessionAffinityFormat: "openai-nosession",
    supportsLongCacheRetention: false,
    supportsStrictMode: true,
    supportsOpenAIGrammarTools: false,
    supportsAdditionalTools: true,
    supportsToolSearch: true,
    supportsExplicitPromptCacheMode: true,
  },
  anthropic: {
    supportsEagerToolInputStreaming: false,
    supportsLongCacheRetention: true,
    sendSessionAffinityHeaders: false,
    supportsCacheControlOnTools: true,
    supportsTemperature: false,
    forceAdaptiveThinking: true,
    allowEmptySignature: true,
    supportsStrictTools: true,
    allowedFallbackModels: [
      {
        provider: "anthropic",
        model: "claude-sonnet-4-6",
        cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
      },
    ],
    supportsToolReferences: false,
  },
  bedrock: { supportsStrictMode: true },
};

const tool: Tool = {
  name: "read_file",
  description: "Read a file",
  parameters: {
    type: "object",
    properties: {
      path: { type: "string", description: "Path" },
      limit: { type: "number" },
    },
    required: ["path"],
  },
};

const context: Context = {
  systemPrompt: "You are pi.",
  messages: [userString, assistantMinimal, toolResult],
  tools: [tool],
};

const goldens: Record<string, unknown> = {
  assistantMessage,
  assistantMinimal,
  assistantDeferred,
  messageWithError,
  userString,
  userBlocks,
  toolResult,
  toolResultError,
  deferredHandle,
  usage,
  messages,
  events,
  model,
  modelCompatSamples,
  tool,
  context,
};

const write = process.argv.includes("--write");
const serialized = Object.fromEntries(
  Object.entries(goldens).map(([name, value]) => [name, JSON.stringify(value, null, "\t")]),
);

if (write) {
  const { mkdirSync, writeFileSync } = await import("node:fs");
  mkdirSync(new URL("../../tests/goldens/ai/", import.meta.url), { recursive: true });
  for (const [name, json] of Object.entries(serialized)) {
    writeFileSync(new URL(`../../tests/goldens/ai/${name}.json`, import.meta.url), `${json}\n`);
  }
  console.log(`wrote ${Object.keys(serialized).length} goldens`);
} else {
  for (const [name, json] of Object.entries(serialized)) {
    console.log(`--- ${name} ---\n${json}`);
  }
}
