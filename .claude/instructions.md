You are working inside the existing repository:

https://github.com/devfernandoa/duet

Your task is to evolve Duet incrementally into a visual multi-agent orchestration application inspired by Maestri.

You have a limited paid token budget. Optimize aggressively for useful code delivered per token.

Do not attempt to rebuild the application from scratch.

Do not perform broad speculative refactors unless they are required for the current milestone.

Do not redesign working systems merely because you would have implemented them differently.

Prefer extending existing abstractions over replacing them.

## Operating rules

Before changing code:

1. Inspect the repository structure.
2. Read the current README.
3. Read Cargo.toml.
4. Identify the files relevant to the requested milestone.
5. Inspect those files directly.
6. Briefly state:
   - what currently exists,
   - what you will reuse,
   - what must change,
   - which files you expect to modify.

Do not produce a long architecture essay.

After that, begin implementation.

## Token-efficiency rules

Minimize unnecessary repository exploration.

Do not repeatedly reread files you already understand unless they have changed.

Do not dump entire large source files into your reasoning.

Search for symbols and inspect targeted ranges instead.

Do not spend tokens explaining standard Rust, GTK, serde, Git, PTY, or Unix concepts unless a design decision depends on them.

Do not create large planning documents unless explicitly requested.

Do not write extensive comments explaining obvious code.

Do not create duplicate abstractions for concepts already present in Duet.

Do not prematurely implement future milestones.

If a clean extension is possible with 50 lines, do not create a 500-line framework.

## Implementation philosophy

Preserve Duet's strengths:

- GTK4/libadwaita desktop UI
- existing infinite canvas
- existing pan/zoom behavior
- draggable/resizable cards
- existing PTY/session infrastructure
- VTE terminal integration
- Claude/Codex support
- session persistence
- notes
- session links
- existing conversation handoff mechanisms

The goal is to turn these existing primitives into more general systems.

The architectural direction should gradually become:

Application
→ Workspace
→ Canvas
→ Nodes and Connections
→ Agents / Notes / Files / Portals
→ Services
→ Runtime / PTY / Filesystem

Avoid coupling application-domain logic directly to GTK widgets when reasonably possible.

UI widgets should display or manipulate domain state rather than becoming the only place that state exists.

## Scope discipline

Work on exactly one milestone at a time.

For every milestone:

1. Inspect relevant existing code.
2. Implement the smallest coherent change.
3. Preserve backwards compatibility when practical.
4. Add or update persistence migrations if required.
5. Compile.
6. Run relevant tests.
7. Fix compilation warnings/errors introduced by your work.
8. Manually reason through critical UI/runtime edge cases.
9. Summarize what changed.
10. Stop.

Do not automatically start the next milestone.

## Validation

For Rust changes, always run the repository's appropriate formatter and compiler checks.

At minimum, when supported by the project:

cargo fmt --check
cargo check

Run tests when present:

cargo test

If formatting fails because of your changes, run:

cargo fmt

Then repeat validation.

Do not claim a feature works if the code has not at least compiled successfully.

If a UI feature cannot be automatically tested, say exactly what should be manually verified.

## Error handling

Do not silently swallow meaningful errors.

Prefer existing project error-handling conventions.

Avoid introducing panics in normal user workflows.

Preserve user workspace/session data whenever possible.

Any persistence schema change should consider data created by older Duet versions.

## Dependencies

Avoid adding dependencies unless they materially simplify the current milestone.

Before adding a crate:

- check whether the repository already has equivalent functionality,
- prefer the standard library when reasonable,
- explain the dependency in one sentence.

Do not add heavyweight frameworks for small features.

## UX principles

The application should remain:

- spatial,
- keyboard-friendly,
- local-first,
- responsive,
- visually understandable,
- multi-agent oriented.

Do not transform Duet into a conventional tabbed chatbot.

The canvas should remain the primary mental model.

## Long-term domain model

Move gradually toward generic canvas objects.

Conceptually:

CanvasNode
- id
- kind
- position
- size
- title
- z-index
- node-specific data

Possible kinds:

- Agent
- Terminal
- Note
- FileTree
- Browser
- Device
- Text
- Group

Connections should eventually also be generic:

Connection
- id
- from
- to
- connection kind

Do not force this final abstraction prematurely if the current milestone can be implemented safely with a smaller migration.

## Agent architecture

Agent providers should eventually support:

- Claude Code
- Codex
- OpenCode
- Shell
- Custom CLI agents

Do not duplicate PTY infrastructure for each provider.

Provider-specific code should focus on:

- executable/command construction,
- resume semantics,
- provider capabilities,
- provider-specific parsing when needed.

## Agent communication

Agent-to-agent communication should eventually use a structured application-level message system rather than blindly forwarding terminal output.

Target conceptually:

AgentMessage
- id
- source agent
- target agent
- message
- timestamp
- status

A lightweight local IPC interface may later expose commands such as:

duet agent list
duet agent send <agent> "<message>"
duet context
duet note read <note>

Do not implement the entire IPC system until the appropriate milestone.

## Maestro architecture

A Maestro is not a separate AI provider.

It is an ordinary agent granted orchestration capabilities.

It should eventually be able to request operations such as:

- create agent
- assign role
- connect agents
- connect notes
- send agent messages
- dismiss agent
- create notes
- inspect agent state

Implement those operations as application APIs/tools, not fragile simulated mouse actions.

## Persistence

User state is important.

When changing models:

- preserve existing sessions where practical,
- preserve card positions,
- preserve notes,
- preserve links,
- version serialized data if necessary,
- add migration logic rather than simply breaking old state.

Never delete user data as part of a schema upgrade without an explicit reason.

## Performance

Duet may host many simultaneous PTYs.

Avoid:

- polling every node at very high frequency,
- cloning large terminal buffers unnecessarily,
- blocking GTK's main thread,
- synchronous filesystem scans on every frame,
- unnecessary canvas redraws,
- recreating widgets for small state updates.

Prefer event-driven updates.

## Security

Treat executable commands and imported configuration as potentially dangerous.

Later features such as templates, custom agents, runtimes, or startup commands must not silently execute untrusted imported content.

Do not introduce network/cloud dependencies for features that can remain local.

## Git behavior

Keep commits or changes logically scoped.

Do not mix unrelated cleanup with feature implementation.

Do not reformat the entire repository unless required.

Do not rename large numbers of files just for stylistic preference.

## Response format after implementation

Keep your final report concise.

Use this format:

Implemented:
- ...
- ...

Files changed:
- ...
- ...

Validation:
- cargo fmt --check: pass/fail
- cargo check: pass/fail
- cargo test: pass/fail/not available

Manual verification:
- ...

Remaining limitations:
- ...

Do not provide a lengthy retrospective.

## Priority roadmap

Implement milestones in this order unless specifically instructed otherwise:

1. Architecture cleanup needed for generic nodes
2. Workspaces
3. Generic agent providers
4. Agent roles
5. Structured agent-to-agent messaging
6. Markdown shared notes
7. Agent context
8. Prompt composer
9. Global search / command palette
10. Agent status and attention
11. Maestro
12. Canvas improvements
13. File tree
14. Git integration
15. Floors using Git worktrees
16. Runtime abstraction
17. Routines
18. Reusable canvas templates
19. Browser portal
20. Chat mode
21. Notebooks
22. Usage meters
23. Remote protocol
24. Mobile client
25. Local AI companion

The target for the first major usable release is milestone 11.

Do not spend the initial budget implementing milestones 12–25 unless specifically requested.

The most valuable product loop is:

Workspace
→ agents with roles
→ shared context
→ agent communication
→ status visibility
→ Maestro orchestration.

Optimize for making that loop excellent first.
