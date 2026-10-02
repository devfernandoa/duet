# Duet Orchestration Platform: Milestone Tracker

Oct 2, 2026 · @F

Duet becomes a Maestri-style spatial orchestration app by evolving the existing Rust/GTK4/libadwaita/VTE codebase in 15 milestones, shipped as five releases and never as a rewrite. Tick items off as they land.

## Overview

The canvas is a persistent graph of nodes and edges; GTK widgets only draw it. Every visible object maps to a domain object, and every agent action goes through the same service layer the GUI uses.

**Releases**

1. v0.2 Orchestration: Milestones 0-5 (foundation, multi-agent core, notes, composer)
2. v0.3 Workspace: Milestones 6-8 (chat, file tree and editor, browser portals)
3. v0.4 Ensemble: Milestones 9-11 (floors, scores, search and attention)
4. v0.5 Automation: Milestones 12-13 (routines, SSH and Docker)
5. v1.0: Milestones 14-15 (remote control, hardening)

**MVP boundary:** infinite canvas, persistent workspaces, multiple agents with roles, agent messaging, agent-writable notes, prompt composer, file tree and one browser portal. Not in the MVP: mobile apps, device emulators, 3D floors, multi-user collaboration, cloud sync, perfect chat extraction from arbitrary CLIs.

**Adjustments to settle before PR 1**

- [x] Put `FloorRef` (Ground or Floor) on `NodeRecord` in schema v4, so Milestone 9 needs no node migration — done in Milestone 1 (`model::FloorRef`, every `NodeRecord` carries one, defaulting to `Ground`); `AgentIdentity` doesn't exist as a separate type yet, so it has nothing to carry a `FloorRef` on until Milestone 3
- [ ] Define `AgentActivity` with an `Unknown` default in Milestone 2; detection arrives in Milestone 3
- [x] Make edges carry a set of capabilities from the start (`Visual` = empty set) — done in Milestone 1 (`model::EdgeRecord::capabilities: BTreeSet<EdgeCapability>`, empty = visual-only); no separate `EdgeKind`/`ConnectionCapability` types existed to merge, so this is the one type that was ever introduced
- [ ] Introduce the `ProjectFilesystem` trait with the Milestone 7 file tree, not Milestone 13
- [ ] Confirm WebKitGTK 6.0 (GTK4) and GtkSourceView 5 are packaged on the target distros

## Phase A: Foundation (Milestones 0-2)

Phase A makes Duet structurally ready for expansion without changing what users see, except that workspaces keep running in the background.

### Milestone 0: Stabilize and refactor

- [ ] Clone `devfernandoa/duet`, branch `feat/orchestration-platform`
- [ ] Install GTK4, libadwaita and VTE dev packages; `cargo fmt --check`, `cargo test`, `cargo run` pass
- [ ] Manual baseline: Claude, Codex, OpenCode and shell terminals; notes; links; move/resize; workspace switch; restart restore; control socket/CLI
- [ ] Explicit storage `schema_version: 2` with migrations; never silently discard old data
- [ ] Split `app.rs` and `main.rs` into `model/`, `runtime/`, `providers/`, `orchestration/`, `persistence/`, `ui/`
- [ ] Done when all existing workflows behave identically and ownership boundaries are clear

### Milestone 1: Generalized canvas engine

- [x] Generic `NodeRecord` and `EdgeRecord`: Terminal, Note, Text, FileTree (placeholder), Portal (placeholder), Drawing (placeholder), Group (placeholder) — schema v4, migrating v3 sessions/notes/links losslessly
- [x] Selection: single, Shift/Ctrl multi, marquee (Shift-drag), select all, deselect (menu + click empty canvas), raise/lower, group move (dragging one of several selected nodes moves all of them together)
- [x] Node operations: move, resize (single-node), duplicate, copy/paste (in-canvas clipboard, not the system clipboard — see below), delete, collapse, lock, front/back — all via the new header "Selection, layout, and undo/redo" menu, deliberately with no keyboard accelerators (see `main.rs`'s `edit_menu_button` doc comment: Ctrl+Z/Ctrl+C/Ctrl+A/Delete are all keys a focused terminal needs unshadowed)
- [x] Canvas operations: pan, zoom, zoom to selection/fit (new), reset, snap-to-grid (new, toggle in the menu)
  - [ ] Minimap — not implemented this milestone. A `DrawingArea` rendering scaled-down node rectangles plus a click/drag-to-pan viewport indicator is a bounded, known-shape addition (reuses `Canvas`'s existing node-position tracking and the same world/screen math already in `canvas.rs`) but didn't fit in this pass; do it as a small follow-up, not a redesign.
  - [ ] Magnetic alignment — explicitly optional per the milestone brief; not implemented.
- [x] Layout: align (left/right/top/bottom), distribute (horizontal/vertical) — pure, tested functions in `layout.rs`, wired through the menu as one `MoveNodes` undo step
  - [ ] "Organize selection" (auto-arrange, e.g. into a grid) — not part of this session's literal requirements and not implemented; align/distribute cover the explicitly-requested layout commands.
- [x] Undo/redo via `CanvasCommand` (`AddNodes`/`RemoveNodes`/`MoveNodes`/`ResizeNode`/`AddEdge`/`RemoveEdge`/`SetProperties`), with a whole drag (move or resize) coalesced into one command at drag-end, never per pointer-motion tick — menu-only (no accelerator), same keyboard-conflict reasoning as above
- [x] Markdown notes: Edit/Preview/Split modes (one `GtkPaned` with both panes always parented, visibility-toggled — avoids the reparenting Split explicitly didn't have to justify), headings/lists/task lists/links/fenced+inline code/blockquotes; tables parse but render as plain rows, not aligned cells (no table widget in a `GtkTextView`); migrated sticky notes open in Preview with their old text as unchanged, valid Markdown source
- Note: edge-creation UI (the link button + click-to-complete gesture) remains `Terminal`-only, matching the pre-Milestone-1 UI exactly — the `EdgeRecord`/`App::create_edge` model itself is generic over any two node ids already, so wiring the same affordance onto other node kinds later needs no model change.

### Milestone 2: Persistent workspaces and runtime survival

- [ ] `RuntimeRegistry` keyed by session id; workspace switch detaches widgets and keeps the process alive
- [ ] `EnvironmentBackend` trait; Local PTY and Local tmux backends; no direct `portable_pty` use outside it
- [ ] Workspace metadata: name, root directory, color/icon, environment, created\_at, last\_opened, viewport, nodes, edges
- [ ] `WorkspaceRuntimeState`: Active, Background, Unloaded
- [ ] Background workspaces show idle / working / awaiting input / completed / failed
- [ ] Explicit actions: unload workspace, restart terminals, terminate one terminal, delete workspace
- [ ] Killing a workspace cannot kill another workspace's processes
- [ ] Acceptance: an agent keeps working in Workspace A while you view B; returning to A shows the same live terminal

## Phase B: Multi-agent core (Milestones 3-5)

Phase B is the first real product release, v0.2 Orchestration: agents that message each other, share notes, and take prompts with rich context.

### Milestone 3: Agent communication and orchestration

- [ ] `orchestration/` service: `bus`, `message`, `registry`, `routing`, `permissions`; `AgentIdentity` separate from terminal widget identity
- [ ] Structured `AgentMessage` with status Queued / Delivered / Acknowledged / Failed
- [ ] Edges express capabilities (`SendMessages`, `ReadNote`, `WriteNote`, `ControlPortal`, `ShareContext`), not PTY piping
- [ ] `AgentRegistry`: list, find by name, get by id, send, connected agents; routing always by UUID
- [ ] `duetctl`: `agents list`, `agents inspect`, `send`, `notes read/write`, `portals list`, `workspace inspect`, `connections list`; GUI and agents use the same service layer
- [ ] Environment metadata on launch: `DUET_WORKSPACE_ID`, `DUET_WORKSPACE_NAME`, `DUET_TERMINAL_ID`, `DUET_AGENT_ID`, `DUET_ROLE`, `DUET_FLOOR_ID`, `DUET_CONTROL_SOCKET`, plus a compact instruction block
- [ ] Per-agent inbox; `AgentAdapter` trait with Claude, Codex, OpenCode, Shell and CustomCommand adapters
- [ ] `AgentActivity`: Starting, Idle, Working, AwaitingUser, AwaitingAgent, Finished, Failed, Offline; show `Unknown` rather than guess
- [ ] Manager role: coordinator with messaging and recruiting; `agents create/remove/assign-role`; only after routing and lifecycle are reliable
- [ ] Acceptance: Lead, Backend, Frontend, Reviewer wired as Lead→Backend, Lead→Frontend, Backend→Reviewer, Frontend→Reviewer complete the delegate-and-summarize task with no manual copying

### Milestone 4: First-class Markdown notes

- [x] Markdown rendering (headings, lists, task lists, links, fenced+inline code, blockquotes; tables parse but render as plain rows) and Edit/Preview/Split modes — pulled forward into Milestone 1 per that session's explicit instruction ("MARKDOWN NOTES — MOVE THIS FEATURE EARLY"); see Milestone 1 above and `markdown.rs`/`node.rs::NoteNode`. Plain-text source editing always available (Edit mode).
- [ ] `NoteNode` with `backing_file`, `NoteSyncMode` (Internal or FileBacked) — not done; `NotePayload` (`model.rs`) has no file-backing fields yet, by design (explicitly deferred, see Milestone 1's own "DO NOT implement yet" list: file-backed notes, filesystem watchers)
- [ ] Agent API: `notes read`, `notes replace`, `notes append` (and `patch`); whole-file overwrite is explicit
- [ ] Connections Note→Note and Terminal→Note; `duetctl notes connections <id>`; never inject the whole note graph into prompts
- [ ] Two-way file sync with filesystem watchers and a content-hash `FileRevision`; never silently overwrite concurrent external changes
- [ ] Acceptance: with `requirements.md`, `implementation.md`, `status.md` connected to Lead, you can watch `status.md` update without editing it

### Milestone 5: Prompt composer and @ mentions

- [ ] Global composer, `Ctrl+Shift+P` (configurable), with a "Send to" target
- [ ] `@` search across agents, notes, files, file trees, portals, kept as typed `PromptReference` values
- [ ] `ContextResolver` pipeline enforcing size limits, permissions, path normalization, local/remote resolution, binary rejection, truncation
- [ ] `PromptAttachment`: drag in, paste, choose, drag a canvas file node
- [ ] Multi-recipient send as separate individual deliveries, no shared conversational state
- [ ] Acceptance: `@backend please compare @requirements with @src/auth.rs and tell @reviewer what you find` resolves with no copy/paste

## Phase C: Daily developer environment (Milestones 6-8)

Phase C is v0.3 Workspace: chat over the terminals, the project filesystem, and a live browser next to the agents.

### Milestone 6: Terminal / chat dual interface

- [ ] Keep the PTY as the execution source; `ShellView` and `ChatView` over one terminal; switching faces never restarts the process
- [ ] `ChatThread` and `ChatMessage` persisted separately from terminal scrollback
- [ ] v1 chat = messages sent through the composer + responses captured by a supported adapter; custom shells fall back to terminal-only
- [ ] Shared rich renderer (Markdown, code blocks, tables, links, images, attachments), reusable by Notes
- [ ] Multiple threads per terminal; thread ids are Duet concepts, not provider conversation ids
- [ ] Acceptance: shell view, chat view, send, formatted reply, back to shell, restart Duet, transcript recovered

### Milestone 7: File tree and embedded editor

- [ ] `FileTreeNode` with independent per-node state; several per workspace; `ProjectFilesystem` trait from the start
- [ ] Expand/collapse, back/forward, collapse all, hidden files, refresh, `.gitignore` support
- [ ] Filename fuzzy search (`Ctrl+P`); content search via `rg` (`>` prefix)
- [ ] GtkSourceView editor: highlighting, line numbers, find/replace, go to line, indentation, save, reload; then tabs, split diff, selected-code to agent
- [ ] Git via CLI: status, diff, diff --cached, log, add, restore, commit; M/A/D/? markers; Stage, Unstage, Discard, Open Diff, Commit with confirmation on dangerous actions
- [ ] Diff to composer reference ("Ask Agent")
- [ ] Drag file to canvas: text becomes a file-backed note, image becomes a FileNode, other binary a generic FileNode
- [ ] Acceptance: browse, see changes, open and edit a file, view its diff, quote part of it, send it to Reviewer, all inside Duet

### Milestone 8: Browser portals

- [ ] `PortalNode` on WebKitGTK: URL field, back, forward, reload, open externally; persist URL, size, identity, storage profile
- [ ] Isolated storage per portal via `PortalProfile`
- [ ] Only a connected terminal may control a portal; `duetctl portal ...` refuses otherwise
- [ ] Automation: `navigate`, `text`, `screenshot`, `evaluate` first; then `reload`, `title`, `click`, `type`
- [ ] Expose current URL, page title and DOM text alongside screenshots
- [ ] Detect dev-server URLs in terminal output and offer "Open in Portal"; never navigate automatically
- [ ] Acceptance: Frontend starts the app, opens it in a connected portal, reads text, interacts, screenshots, edits code, reloads and verifies, with the browser visible throughout

## Phase D: Parallel work (Milestones 9-11)

Phase D is v0.4 Ensemble: isolated floors, reusable team arrangements, and keyboard navigation for a large canvas.

### Milestone 9: Floors (git-isolated work)

- [ ] `Floor` model; `FloorRef` enum (Ground or Floor(id)); every node carries `workspace_id` and a floor ref
- [ ] Create via `git worktree add -b duet/floor/<id> <path> <base>`; store the exact worktree path, never rebuild it from the name
- [ ] Optional "Clone Ground Layout": nodes, connections, roles, notes, portal definitions, file trees; never PIDs, PTY handles, statuses or temp files
- [ ] Floor switcher (sidebar or bottom bar); 3D view not required
- [ ] Landing: check floor clean, ground clean, merge possible; stop without touching either tree if not; never auto-resolve conflicts, show them
- [ ] Hooks `setup`, `run`, `teardown` with `DUET_PROJECT_NAME`, `DUET_FLOOR_NAME`, `DUET_BRANCH_NAME`, `DUET_FLOOR_PATH`, `DUET_ROOT_PATH`
- [ ] Acceptance: Ground on `main`, Floor A on `feature/auth`, Floor B on `fix/sidebar`, agents working in parallel, no change leaks until landed

### Milestone 10: Reusable arrangements (Scores)

- [ ] Serialize a selection (nodes plus internal edges) with coordinates relative to the selection origin
- [ ] Strip scrollback, PIDs, tokens, env secrets, SSH credentials, absolute paths (use `$WORKSPACE_ROOT`), cookies, portal auth storage, socket names
- [ ] Versioned format `{"format": "duet-score", "version": 1, ...}`; Export, Import, Duplicate, Rename, Delete
- [ ] Starter Scores: Solo Developer, Lead + Implementer + Reviewer, Frontend Team, Backend Team, Bug Investigation, PR Review
- [ ] Acceptance: save Lead, Coder, Tester, Reviewer, status note, file tree and portal as a Score; insert it in another workspace with roles and connections intact

### Milestone 11: Search, command palette and attention

- [ ] `Ctrl+P` palette over commands, workspaces, floors, agents, notes, files, portals, scores; selecting navigates workspace, floor, canvas pan, selection, focus
- [ ] Fuzzy matching via a library, over a `SearchIndex`
- [ ] Chat search indexed separately; results jump to the exact message
- [ ] `agents_needing_attention()` plus "next agent needing attention" and "all" views (finished, asked a question, failed, waiting for permission)
- [ ] Acceptance: with 8 workspaces, 10 floors, 40 agents and 100 notes, any node or waiting agent is reachable from the keyboard within seconds

## Phases E and F: Automation, environments, remote, hardening (Milestones 12-15)

These milestones depend on a reliable agent lifecycle (Milestone 3) and on `ProjectFilesystem` (Milestone 7); do not start them earlier.

### Milestone 12: Routines

- [ ] `Routine` model targeting stable ids (workspace, floor, terminal, agent) with a `Schedule`
- [ ] Schedules: every N minutes, every N hours, daily at HH:MM (cron later)
- [ ] Structured `RoutineStep { prompt, wait_for_completion }`; the UI may accept `&&` but stores steps
- [ ] Completion gating from the `AgentActivity` model: step 2 waits for step 1
- [ ] `MissedRunPolicy` Skip (default) or RunLatest; never replay every missed run
- [ ] History records start, finish, target, result, error; no full transcripts
- [ ] Acceptance: hourly Reviewer routine summarizes new commits into `@review-status` while its workspace is in the background

### Milestone 13: Environment abstraction

- [ ] `Environment` trait (spawn, execute, read/write file, list, upload, download); Local, SSH, Docker implementations
- [ ] Workspace default environment, inherited by terminals unless overridden
- [ ] SSH through OpenSSH and the user's config (`~/.ssh/config`, ssh-agent, IdentityFile, ProxyJump); no custom credential manager
- [ ] Docker via `docker exec` and `docker cp`, identity `docker:<container-id>`
- [ ] File trees and editor go through `ProjectFilesystem`, never `std::fs` directly
- [ ] Remote attachments: upload to a remote temp path, reference it in the prompt, clean up opportunistically
- [ ] Acceptance: in an SSH workspace, terminals, note references, file tree, editor, roles and messaging all work

### Milestone 14: Remote control (post-MVP)

- [ ] Optional HTTP API plus WebSocket event feed over the same application services as GUI and CLI
- [ ] Localhost only first; then pairing with short-lived code, device key, revocable authorization
- [ ] Roles Viewer, Controller, Owner; deleting a workspace, landing a floor, running a routine and terminating an agent require Owner
- [ ] First client: workspace list, agent status, chat, send prompt, notifications; no full canvas

### Milestone 15: Product hardening

- [ ] Atomic saves (temp file, fsync, rename), last known-good copy, corruption detection, no silent reset
- [ ] Runtime recovery: reconnect to live tmux sessions, show stopped otherwise, never auto-spawn duplicates
- [ ] Structured logging for sessions, messages, workspace switches, floor git operations, portal automation, routines, migrations; no tokens, credentials, cookies or prompts by default
- [ ] Performance pass at 100 nodes, 30 terminals, 10 live agents, several portals, large notes and trees; suspend offscreen rendering, never agent processes
- [ ] Move chat, routine history, search and events to SQLite when volume justifies it
- [ ] Workspace Export/Import (backup) kept separate from Scores (sanitized templates)
- [ ] Security audit: command injection, path and symlink traversal, remote operations, portal JavaScript, agent-to-agent and agent-to-portal authorization, uploads, score and workspace imports, remote API

## First 10 PRs and working rules

The first ten PRs reach v0.2, and each one is reviewable on its own.

- [ ] PR 1: `refactor: separate persisted model from GTK runtime` (no behavior change)
- [ ] PR 2: `feat: versioned state schema and migrations` (with migration tests)
- [x] PR 3: `refactor: generic NodeRecord and EdgeRecord` (migrate terminals and sticky notes)
- [x] PR 4: `feat: selection and multi-node canvas operations`
- [ ] PR 5: `refactor: RuntimeRegistry independent of active workspace` (critical checkpoint: switching no longer destroys PTYs)
- [ ] PR 6: `feat: agent registry and structured message bus` (preserve existing messaging behavior)
- [ ] PR 7: `feat: duetctl agent messaging API` (`agents list`, `agents inspect`, `send`)
- [ ] PR 8: `feat: agent-readable and writable markdown notes`
- [ ] PR 9: `feat: connection capabilities` (edges become explicit access relationships)
- [ ] PR 10: `feat: global prompt composer with @ mentions` (cut v0.2)

### Five questions before every feature

1. What is its persisted model? (for example `PortalRecord`)
2. What is its runtime state? (`WebKitWebView`)
3. Which application service owns its behavior? (`PortalService`)
4. How does GTK invoke it? (`PortalService::navigate()`)
5. How do agents and the CLI invoke the same behavior? (`duetctl portal navigate`)

If answering question 5 means reimplementing the behavior, the architecture is wrong. Dependencies point one way: Model, then application services, then GTK / CLI / remote API. Never Model to GTK, Orchestration to GTK, Git to canvas widgets, or Persistence to terminal widgets.

### Testing strategy

- [ ] Model tests: workspace, floor, node and edge serialization and migration; score sanitization; role resolution
- [ ] Orchestration tests with a `FakeAgentAdapter`: delivery, failure, queueing, acknowledgement, permissions, manager delegation
- [ ] Filesystem tests in temp dirs: note sync, file tree, search, git operations, floor create and land
- [ ] Portal tests against a deterministic local test site, never public websites
- [ ] Migration fixtures for every released store version (`fixtures/store-v1.json`, `v2`, `v3`): old, migrate, load, save, reload

## Functional parity checklist

Progress against the Maestri-style target is measured by capability, not by visual similarity.

**Canvas:**

- [x] Infinite pan and zoom
- [x] Multi-select, marquee, lock
  - [ ] Groups — `Group` exists only as a placeholder node kind (no containment/move-as-unit semantics beyond what multi-select already gives); real grouping is unimplemented.
- [x] Align, distribute
- [x] Undo, redo
  - [ ] Minimap — not implemented (see Milestone 1 above for the planned shape).
  - [ ] Drawings — `Drawing` exists only as a placeholder node kind; no freehand drawing.

**Workspaces:**

- [ ] Multiple projects, saved layouts
- [ ] Background execution, switching, unload, duplication

**Terminals:**

- [ ] Claude, Codex, OpenCode, shell, custom command
- [ ] Persistent sessions, named terminals, roles, activity status

**Orchestration:**

- [ ] Agent registry, structured messages, connections
- [ ] Delivery status, manager role, dynamic recruitment, CLI

**Notes:**

- [x] Markdown with preview
- [ ] Agent readable and writable, note graph
- [ ] File-backed notes with two-way sync

**Composer:**

- [ ] Agent, note, file and portal mentions
- [ ] Attachments, multi-agent send

**Chat:**

- [ ] Shell/chat switch, persistent history, threads
- [ ] Markdown responses, code blocks, attachments, chat search

**Files:**

- [ ] File tree, fuzzy and content search
- [ ] Embedded editor with syntax highlighting
- [ ] Git status, diff, stage/unstage, send selection to agent

**Portals:**

- [ ] Embedded browser, navigation, isolated storage
- [ ] Agent connection, navigation, DOM access, interaction, screenshots

**Floors:**

- [ ] Multiple floors with git worktree isolation, branch creation
- [ ] Clone layout, hooks, landing, conflict detection

**Reusable arrangements:**

- [ ] Save and restore a selection with relative coordinates
- [ ] Share file, secret stripping, preview

**Search:**

- [ ] Nodes, workspaces, floors, agents, notes, files, chats, commands
- [ ] Attention queue

**Automation:**

- [ ] Scheduler, agent targets, sequential steps
- [ ] Background execution, pause, history

**Environments:**

- [ ] Local, SSH, Docker
- [ ] Remote files, remote uploads

**Remote:**

- [ ] Authenticated API, event stream, device pairing
- [ ] Agent status, chat, send prompts, workspace management

## Clone Complete: the end-to-end workflow

The project is done when one person can run all eighteen steps naturally.

- [ ] Open a project as a persistent workspace
- [ ] Create Lead, Backend, Frontend, Tester and Reviewer
- [ ] Assign each a reusable role
- [ ] Arrange the agents around project notes and files
- [ ] Connect agents so they communicate without copy/paste
- [ ] Give the Lead a task
- [ ] Lead delegates the work
- [ ] Backend and Frontend work simultaneously
- [ ] Notes update with shared project status
- [ ] Inspect changed files and diffs on the canvas
- [ ] Frontend opens and inspects the app in a browser portal
- [ ] Reviewer inspects the other agents' work
- [ ] Create an isolated Floor for an alternative implementation
- [ ] Run both approaches at once without touching Ground
- [ ] Land the chosen implementation
- [ ] Save the team arrangement as a Score
- [ ] Open another project and instantiate the Score
- [ ] Close and reopen Duet without losing arrangement or durable agent context
