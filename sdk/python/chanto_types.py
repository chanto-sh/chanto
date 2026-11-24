"""Generated from sdk/schema.json by sdk/gen_types.py: don't edit by hand."""

from typing import Any, Dict, List, Optional, Union

try:
    from typing import Literal, TypedDict
except ImportError:  # Python 3.7
    from typing_extensions import Literal, TypedDict  # type: ignore


class _CreateSessionRequired(TypedDict):
    pass


class CreateSession(_CreateSessionRequired, total=False):
    """`POST /sessions`: start a session (every field optional)."""
    allowed_tools: Optional[List[str]]
    cwd: Optional[str]
    """The working directory (the server's own by default)."""
    disallowed_tools: Optional[List[str]]
    max_turns: Optional[int]
    mode: Union["Mode", Optional[Any]]
    model: Optional[str]
    resume: Optional[str]
    """A saved session's id to continue."""
    system_prompt_append: Optional[str]
    tools: Optional[List["ToolSpec"]]
    """Tools the client runs itself: the agent calls them with `tool_call`."""
    verify: Optional[bool]
    """Run the project's checks before finishing, as `--verify` does."""


Decision = Literal["allow", "allow_always", "deny"]


class _ForkRequired(TypedDict):
    pass


class Fork(_ForkRequired, total=False):
    """`POST /sessions/{id}/fork`: a live copy of the session, from now or from
    a checkpoint.
    """
    name: Optional[str]
    point: Optional[int]
    """A checkpoint index; the current conversation when omitted."""


class Health(TypedDict):
    """`GET /health`."""
    ok: bool
    runs: bool
    """Whether this server hosts background runs (`/runs`)."""
    version: str
    """The server's Chanto version."""


Mode = Literal["default", "acceptEdits", "plan", "auto", "bypassPermissions"]
# A permission mode.


class _RespondRequired(TypedDict):
    pass


class Respond(_RespondRequired, total=False):
    """`POST /sessions/{id}/respond`: answers a `permission_request` (`decision`,
    optionally `feedback`), a `question` (`answer`), or a `tool_call` (`id`
    and `result`).
    """
    answer: Optional[str]
    decision: Union["Decision", Optional[Any]]
    feedback: Optional[str]
    id: Optional[str]
    """A `tool_call`'s id."""
    request_id: Optional[str]
    result: Union["ToolResult", Optional[Any]]


class _RewindRequired(TypedDict):
    pass


class Rewind(_RewindRequired, total=False):
    """`POST /sessions/{id}/rewind`: back to a checkpoint, or undo the last
    rewind's code restore. With `preview`, only reports what it would do.
    """
    checkpoint: Optional[int]
    mode: Union["RewindMode", Optional[Any]]
    preview: Optional[bool]
    undo: Optional[bool]


RewindMode = Literal["both", "conversation", "code"]


class SendMessage(TypedDict):
    """`POST /sessions/{id}/messages`: a prompt or a `/command`."""
    text: str


class SessionCreated(TypedDict):
    """`POST /sessions` answer."""
    id: str
    model: str
    warnings: List[str]


class Todo(TypedDict):
    """A todo-list item."""
    activeForm: str
    content: str
    status: str
    """`pending`, `in_progress` or `completed`."""


class ToolResult(TypedDict):
    """What a client-side tool returned."""
    content: str
    is_error: bool


class ToolSpec(TypedDict):
    """A client-side tool: the agent sees its name, description and the JSON
    Schema of its input (an object schema).
    """
    description: str
    input_schema: Any
    name: str


class _TurnDoneRequired(TypedDict):
    price_unknown: bool
    total_cost_usd: float


class TurnDone(_TurnDoneRequired, total=False):
    """The end of a prompt or command: its answer (`result`), a command's printed
    text (`output`), an `error`, or `interrupted`, and the session's cost so far.
    """
    error: Optional[str]
    interrupted: Optional[bool]
    output: Optional[str]
    result: Optional[str]


class _WindowRequired(TypedDict):
    used_percent: float


class Window(_WindowRequired, total=False):
    """One usage-limit window (the 5-hour rolling one or the weekly one)."""
    resets_at: Optional[int]
    """Unix seconds."""
    window_minutes: Optional[int]


class AssistantStartEvent(TypedDict):
    """The model started a reply."""
    type: Literal["assistant_start"]


class AssistantEndEvent(TypedDict):
    """The model's reply ended."""
    type: Literal["assistant_end"]


class TextEvent(TypedDict):
    """Reply text as it streams in."""
    type: Literal["text"]
    text: str


class ReasoningEvent(TypedDict):
    """Reasoning as it streams in."""
    type: Literal["reasoning"]
    text: str


class _ToolStartEventRequired(TypedDict):
    type: Literal["tool_start"]
    id: str
    label: str


class ToolStartEvent(_ToolStartEventRequired, total=False):
    """A tool call started; `parent` is the subagent call it belongs to."""
    parent: Optional[str]


class _ToolEndEventRequired(TypedDict):
    type: Literal["tool_end"]
    display: str
    id: str
    images: List[str]
    """Up to two images the tool returned, as data URLs."""
    is_error: bool
    output: str


class ToolEndEvent(_ToolEndEventRequired, total=False):
    """A tool call finished. `output` is what the model saw (secrets
    redacted, at most 2,000 characters); `display` is the human-facing
    summary, edit diffs included.
    """
    parent: Optional[str]


class ToolProgressEvent(TypedDict):
    """Output from a tool that's still running."""
    type: Literal["tool_progress"]
    id: str
    text: str


class _BusyEventRequired(TypedDict):
    type: Literal["busy"]


class BusyEvent(_BusyEventRequired, total=False):
    """What the agent is busy with, or `null` when it's idle."""
    label: Optional[str]


class UsageEvent(TypedDict):
    """Token use and cost so far."""
    type: Literal["usage"]
    context_max: int
    context_used: int
    price_unknown: bool
    """True when some model's price isn't known, so the cost is a floor."""
    total_cost_usd: float
    total_in: int
    total_out: int
    tps: float
    """Tokens per second of the last reply."""


class WarningEvent(TypedDict):
    type: Literal["warning"]
    message: str


class ErrorEvent(TypedDict):
    type: Literal["error"]
    message: str


class InfoEvent(TypedDict):
    type: Literal["info"]
    message: str


class TodosEvent(TypedDict):
    """The session's todo list, in full."""
    type: Literal["todos"]
    todos: List["Todo"]


class ModeEvent(TypedDict):
    """The permission mode changed."""
    type: Literal["mode"]
    mode: "Mode"


class CompactedEvent(TypedDict):
    """The conversation was compacted from `before` to `after` tokens."""
    type: Literal["compacted"]
    after: int
    before: int


class BackgroundAgentEvent(TypedDict):
    """A background subagent started or changed state."""
    type: Literal["background_agent"]
    description: str
    id: str
    status: str


class _RateLimitsEventRequired(TypedDict):
    type: Literal["rate_limits"]
    summary: str


class RateLimitsEvent(_RateLimitsEventRequired, total=False):
    """A ChatGPT plan's usage limits."""
    plan: Optional[str]
    primary: Union["Window", Optional[Any]]
    secondary: Union["Window", Optional[Any]]


class StatusEvent(TypedDict):
    """Live status of a long-running mode, by key ("goal", "team")."""
    type: Literal["status"]
    key: str
    lines: List[str]


class _PermissionRequestEventRequired(TypedDict):
    type: Literal["permission_request"]
    always_label: str
    """What "allow always" would allow."""
    detail: str
    label: str
    request_id: str
    tool: str


class PermissionRequestEvent(_PermissionRequestEventRequired, total=False):
    """The agent wants to use a tool and needs a decision: answer with
    `POST /sessions/{id}/respond` and `{request_id, decision, feedback?}`.
    """
    reason: Optional[str]


class QuestionEvent(TypedDict):
    """The agent asks a question: answer with `{request_id, answer}`."""
    type: Literal["question"]
    options: List[str]
    question: str
    request_id: str


class ToolCallEvent(TypedDict):
    """The agent called a tool the session was created with: answer with
    `{id, result: {content, is_error}}` within ten minutes.
    """
    type: Literal["tool_call"]
    id: str
    input: Any
    name: str


class _TurnDoneEventRequired(TypedDict):
    type: Literal["turn_done"]
    price_unknown: bool
    total_cost_usd: float


class TurnDoneEvent(_TurnDoneEventRequired, total=False):
    """A prompt or command finished."""
    error: Optional[str]
    interrupted: Optional[bool]
    output: Optional[str]
    result: Optional[str]


StreamEvent = Union[
    AssistantStartEvent,
    AssistantEndEvent,
    TextEvent,
    ReasoningEvent,
    ToolStartEvent,
    ToolEndEvent,
    ToolProgressEvent,
    BusyEvent,
    UsageEvent,
    WarningEvent,
    ErrorEvent,
    InfoEvent,
    TodosEvent,
    ModeEvent,
    CompactedEvent,
    BackgroundAgentEvent,
    RateLimitsEvent,
    StatusEvent,
    PermissionRequestEvent,
    QuestionEvent,
    ToolCallEvent,
    TurnDoneEvent,
]
# One event on `GET /sessions/{id}/events` (also `--output-format
# stream-json`): a JSON object whose `type` names the variant.
