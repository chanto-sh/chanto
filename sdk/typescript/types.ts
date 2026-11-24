// Generated from sdk/schema.json by sdk/gen_types.py: don't edit by hand.

/** `POST /sessions`: start a session (every field optional). */
export interface CreateSession {
  allowed_tools?: string[] | null;
  /** The working directory (the server's own by default). */
  cwd?: string | null;
  disallowed_tools?: string[] | null;
  max_turns?: number | null;
  mode?: Mode | null;
  model?: string | null;
  /** A saved session's id to continue. */
  resume?: string | null;
  system_prompt_append?: string | null;
  /** Tools the client runs itself: the agent calls them with `tool_call`. */
  tools?: ToolSpec[] | null;
  /** Run the project's checks before finishing, as `--verify` does. */
  verify?: boolean | null;
}

export type Decision = "allow" | "allow_always" | "deny";

/**
 * `POST /sessions/{id}/fork`: a live copy of the session, from now or from
 * a checkpoint.
 */
export interface Fork {
  name?: string | null;
  /** A checkpoint index; the current conversation when omitted. */
  point?: number | null;
}

/** `GET /health`. */
export interface Health {
  ok: boolean;
  /** Whether this server hosts background runs (`/runs`). */
  runs: boolean;
  /** The server's Chanto version. */
  version: string;
}

/** A permission mode. */
export type Mode = "default" | "acceptEdits" | "plan" | "auto" | "bypassPermissions";

/**
 * `POST /sessions/{id}/respond`: answers a `permission_request` (`decision`,
 * optionally `feedback`), a `question` (`answer`), or a `tool_call` (`id`
 * and `result`).
 */
export interface Respond {
  answer?: string | null;
  decision?: Decision | null;
  feedback?: string | null;
  /** A `tool_call`'s id. */
  id?: string | null;
  request_id?: string | null;
  result?: ToolResult | null;
}

/**
 * `POST /sessions/{id}/rewind`: back to a checkpoint, or undo the last
 * rewind's code restore. With `preview`, only reports what it would do.
 */
export interface Rewind {
  checkpoint?: number | null;
  mode?: RewindMode | null;
  preview?: boolean | null;
  undo?: boolean | null;
}

export type RewindMode = "both" | "conversation" | "code";

/** `POST /sessions/{id}/messages`: a prompt or a `/command`. */
export interface SendMessage {
  text: string;
}

/** `POST /sessions` answer. */
export interface SessionCreated {
  id: string;
  model: string;
  warnings: string[];
}

/** A todo-list item. */
export interface Todo {
  activeForm: string;
  content: string;
  /** `pending`, `in_progress` or `completed`. */
  status: string;
}

/** What a client-side tool returned. */
export interface ToolResult {
  content: string;
  is_error: boolean;
}

/**
 * A client-side tool: the agent sees its name, description and the JSON
 * Schema of its input (an object schema).
 */
export interface ToolSpec {
  description: string;
  input_schema: unknown;
  name: string;
}

/**
 * The end of a prompt or command: its answer (`result`), a command's printed
 * text (`output`), an `error`, or `interrupted`, and the session's cost so far.
 */
export interface TurnDone {
  error?: string | null;
  interrupted?: boolean | null;
  output?: string | null;
  price_unknown: boolean;
  result?: string | null;
  total_cost_usd: number;
}

/** One usage-limit window (the 5-hour rolling one or the weekly one). */
export interface Window {
  /** Unix seconds. */
  resets_at?: number | null;
  used_percent: number;
  window_minutes?: number | null;
}

/** The model started a reply. */
export interface AssistantStartEvent {
  type: "assistant_start";
}

/** The model's reply ended. */
export interface AssistantEndEvent {
  type: "assistant_end";
}

/** Reply text as it streams in. */
export interface TextEvent {
  type: "text";
  text: string;
}

/** Reasoning as it streams in. */
export interface ReasoningEvent {
  type: "reasoning";
  text: string;
}

/** A tool call started; `parent` is the subagent call it belongs to. */
export interface ToolStartEvent {
  type: "tool_start";
  id: string;
  label: string;
  parent?: string | null;
}

/**
 * A tool call finished. `output` is what the model saw (secrets
 * redacted, at most 2,000 characters); `display` is the human-facing
 * summary, edit diffs included.
 */
export interface ToolEndEvent {
  type: "tool_end";
  display: string;
  id: string;
  /** Up to two images the tool returned, as data URLs. */
  images: string[];
  is_error: boolean;
  output: string;
  parent?: string | null;
}

/** Output from a tool that's still running. */
export interface ToolProgressEvent {
  type: "tool_progress";
  id: string;
  text: string;
}

/** What the agent is busy with, or `null` when it's idle. */
export interface BusyEvent {
  type: "busy";
  label?: string | null;
}

/** Token use and cost so far. */
export interface UsageEvent {
  type: "usage";
  context_max: number;
  context_used: number;
  /** True when some model's price isn't known, so the cost is a floor. */
  price_unknown: boolean;
  total_cost_usd: number;
  total_in: number;
  total_out: number;
  /** Tokens per second of the last reply. */
  tps: number;
}

export interface WarningEvent {
  type: "warning";
  message: string;
}

export interface ErrorEvent {
  type: "error";
  message: string;
}

export interface InfoEvent {
  type: "info";
  message: string;
}

/** The session's todo list, in full. */
export interface TodosEvent {
  type: "todos";
  todos: Todo[];
}

/** The permission mode changed. */
export interface ModeEvent {
  type: "mode";
  mode: Mode;
}

/** The conversation was compacted from `before` to `after` tokens. */
export interface CompactedEvent {
  type: "compacted";
  after: number;
  before: number;
}

/** A background subagent started or changed state. */
export interface BackgroundAgentEvent {
  type: "background_agent";
  description: string;
  id: string;
  status: string;
}

/** A ChatGPT plan's usage limits. */
export interface RateLimitsEvent {
  type: "rate_limits";
  plan?: string | null;
  primary?: Window | null;
  secondary?: Window | null;
  summary: string;
}

/** Live status of a long-running mode, by key ("goal", "team"). */
export interface StatusEvent {
  type: "status";
  key: string;
  lines: string[];
}

/**
 * The agent wants to use a tool and needs a decision: answer with
 * `POST /sessions/{id}/respond` and `{request_id, decision, feedback?}`.
 */
export interface PermissionRequestEvent {
  type: "permission_request";
  /** What "allow always" would allow. */
  always_label: string;
  detail: string;
  label: string;
  reason?: string | null;
  request_id: string;
  tool: string;
}

/** The agent asks a question: answer with `{request_id, answer}`. */
export interface QuestionEvent {
  type: "question";
  options: string[];
  question: string;
  request_id: string;
}

/**
 * The agent called a tool the session was created with: answer with
 * `{id, result: {content, is_error}}` within ten minutes.
 */
export interface ToolCallEvent {
  type: "tool_call";
  id: string;
  input: unknown;
  name: string;
}

/** A prompt or command finished. */
export interface TurnDoneEvent {
  type: "turn_done";
  error?: string | null;
  interrupted?: boolean | null;
  output?: string | null;
  price_unknown: boolean;
  result?: string | null;
  total_cost_usd: number;
}

/**
 * One event on `GET /sessions/{id}/events` (also `--output-format
 * stream-json`): a JSON object whose `type` names the variant.
 */
export type StreamEvent =
  | AssistantStartEvent
  | AssistantEndEvent
  | TextEvent
  | ReasoningEvent
  | ToolStartEvent
  | ToolEndEvent
  | ToolProgressEvent
  | BusyEvent
  | UsageEvent
  | WarningEvent
  | ErrorEvent
  | InfoEvent
  | TodosEvent
  | ModeEvent
  | CompactedEvent
  | BackgroundAgentEvent
  | RateLimitsEvent
  | StatusEvent
  | PermissionRequestEvent
  | QuestionEvent
  | ToolCallEvent
  | TurnDoneEvent;
