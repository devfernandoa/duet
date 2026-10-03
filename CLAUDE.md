# Duet Development Instructions

You are working on Duet, an existing Rust desktop application built with GTK4,
libadwaita and VTE.

The long-term objective is to evolve Duet into a spatial multi-agent
orchestration environment inspired by the workflows of Maestri.

This is NOT a rewrite.

The existing Duet codebase is the foundation and must be evolved incrementally.

Read `steps.md` before making architectural decisions. It contains the complete
roadmap and acceptance criteria.

Duet 1.0 is Milestones 0-6, 8 and 7.5. Everything in steps.md's
"Post-v1 / Future" section (chat, floors, scores, search/attention, routines,
SSH/Docker, remote control, hardening) is future work: do not start it unless
a task explicitly asks for it.

## Core architectural direction

Duet should evolve toward this dependency direction:

    Domain model
        ↓
    Application services
        ↓
    Runtime / integrations
        ↓
    GTK UI / CLI / future remote API

Avoid dependencies in the opposite direction.

In particular, avoid designs such as:

    Model → GTK
    Orchestration → GTK widgets
    Git logic → canvas widgets
    Persistence → terminal widgets

GTK widgets are presentation objects, not authoritative application state.

## Product model

The canvas should eventually represent a persistent graph of nodes and edges.

Important node types include:

    Terminal
    Note
    Text
    FileTree
    Portal
    Drawing
    Group

Every visible canvas object should correspond to a domain object.

Connections between nodes should eventually represent explicit capabilities,
not just visual lines.

Examples include:

    SendMessages
    ReadNote
    WriteNote
    ControlPortal
    ShareContext

A visual-only edge is allowed and should have no capabilities.

## Existing behavior is valuable

Before modifying an area:

1. Read its implementation.
2. Understand the existing behavior.
3. Search for all call sites.
4. Identify persistence implications.
5. Identify runtime implications.
6. Identify GTK/UI implications.
7. Check whether tests already exist.

Do not replace working behavior merely because another design looks cleaner.

Preserve existing functionality unless the current task explicitly changes it.

## Incremental development rule

Only implement the task I give you.

Do not implement later roadmap features preemptively.

You may introduce a small abstraction needed by the current feature if it
clearly prevents technical debt, but do not turn a small task into a broad
rewrite.

If you discover something important that belongs to a future milestone:

- document it;
- explain it at the end;
- do not implement it unless required by the current task.

## Before coding

For every task:

1. Read `steps.md`.
2. Inspect relevant source files.
3. Summarize the current implementation.
4. Explain the smallest safe implementation plan.
5. Identify compatibility or migration concerns.
6. Then implement.

Do not start editing immediately without first understanding the affected code.

## Persistence

Persisted user workspaces are high-value data.

Never silently discard or reset persisted data.

All schema changes must:

- have an explicit schema version;
- preserve existing user data where practical;
- include migration logic;
- include migration tests once the migration system exists.

Stable IDs should be used internally.

Human-readable names are not identities.

## Runtime state

Keep persisted state separate from runtime state.

Persisted objects include things such as:

    Workspace
    Node
    Edge
    Role
    Floor
    Score
    Routine

Runtime objects include things such as:

    PTY processes
    child processes
    tmux sessions
    WebKit views
    timers
    sockets
    live filesystem watchers

Do not serialize runtime handles into workspace state.

## Notes and Markdown

Markdown support is an EARLY foundational feature.

During the canvas foundation work, Note nodes should support:

- Markdown source editing
- headings
- lists
- task lists
- links
- fenced code blocks
- inline code
- blockquotes
- tables if the selected Markdown library supports them reliably
- Edit mode
- Preview mode

Split mode can come immediately afterward if it does not create excessive UI
complexity.

Plain-text source editing must always remain available.

The early Markdown implementation does NOT need:

- agent read/write access
- file-backed synchronization
- note graphs
- filesystem watchers

Those belong to the later advanced Notes milestone.

Design the Markdown renderer so it can eventually also be reused by Chat.

Do not build two unrelated Markdown rendering systems for Notes and Chat.

## Agent architecture

Do not assume an agent is the same thing as a GTK terminal widget.

Eventually:

    AgentIdentity
        ↕
    AgentAdapter
        ↕
    terminal/runtime

Provider-specific behavior should live behind adapters.

Expected providers include:

    Claude
    Codex
    OpenCode
    Shell
    CustomCommand

Do not spread Claude-specific or Codex-specific assumptions throughout the
application.

## Claude Code priority

Claude Code is the primary development/testing agent for this project.

When implementing orchestration features, make sure the design works cleanly
with Claude Code first.

Do not break the abstractions required by Codex/OpenCode, but Claude Code is
the first integration that should be proven end-to-end.

## CLI and GUI

Long term, GUI actions and agent/CLI actions should invoke the same application
services.

Avoid implementing one behavior separately in GTK and again in `duetctl`.

Prefer:

    WorkspaceService
    NoteService
    AgentService
    PortalService

called by:

    GTK
    duetctl
    agents
    future remote API

## Testing

Prefer testing below the GTK layer.

Add tests for:

- serialization
- migrations
- orchestration
- message routing
- filesystem operations
- Git operations
- sanitization

Use GTK integration tests only when the behavior genuinely requires GTK.

Never depend on public internet services for automated tests.

## Git workflow

Before finishing a task:

1. Run `cargo fmt`.
2. Run relevant tests.
3. Run `cargo check`.
4. Run the full test suite when practical.
5. Inspect `git diff`.
6. Check for unrelated edits.
7. Check for obvious warnings or dead code.

Do not automatically commit unless I explicitly tell you to commit.

At the end, suggest a concise conventional commit message.

## Finishing every task

At the end of each task report:

### Implemented
What changed.

### Files changed
Important files touched.

### Tests
Commands run and their result.

### Manual verification
Anything I should verify visually or interactively.

### Architecture notes
Important decisions or future implications.

### Roadmap
Which `steps.md` checkboxes can now be marked complete.

### Suggested commit
A conventional commit message.

If the acceptance criteria are not fully met, explicitly say what remains.

Do not claim a milestone or checkbox is complete unless it actually is.
