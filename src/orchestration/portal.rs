//! Portal domain service (Milestone 8): everything about a browser Portal
//! that doesn't need WebKit or GTK — URL validation, `ControlPortal`
//! authorization, which portals an agent can discover, where a profile's
//! storage lives, the exact scripts Duet runs inside a page for
//! text/click/type, how their answers are parsed, dev-server URL detection
//! in terminal output, and screenshot file management. `portal_runtime`
//! owns the live `WebView`s and `app::portals` (the `PortalService`) wires
//! the two together for GTK and `duetctl` alike — the same split
//! `orchestration::notes` / `App`'s note methods already use.
//!
//! Authorization follows `notes::authorize_note`: an agent-issued request
//! (`actor = Some`) needs a `ControlPortal` edge to the portal; the human
//! operator (`None` — the GUI or a bare CLI call) is trusted, the same level
//! `MessageBus::send` and note editing already give it. Being in the same
//! workspace grants nothing. Arbitrary script evaluation additionally needs
//! the portal's own `allow_scripts` opt-in, so `ControlPortal` alone never
//! means "may run any code in this page".

use super::permissions::authorize;
use crate::model::{EdgeCapability, EdgeRecord, NodeRecord, PortalProfile, PortalStorage};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Turns what a human or agent typed into a URL a portal may load, or a
/// clear refusal. Only `http`/`https` (and `about:blank`) are allowed:
/// `file:`, `javascript:`, `data:` and friends would let a "navigate" read
/// local files or run code, which is not what navigation is for. A bare
/// `localhost:3000`/`127.0.0.1:5173/x` becomes `http://...`; any other bare
/// host (`example.com`) becomes `https://...`. `0.0.0.0` (what many dev
/// servers bind to) is rewritten to `localhost`, the address a browser can
/// actually connect to.
pub fn normalize_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("a URL is required".to_string());
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(format!("'{raw}' is not a URL (it contains whitespace)"));
    }
    if raw.eq_ignore_ascii_case("about:blank") {
        return Ok("about:blank".to_string());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => {
            // `scheme:` without `//` (javascript:, data:, mailto:, about:...)
            // — but `localhost:3000` also has a colon, so only treat it as a
            // scheme if what follows the colon isn't a port.
            if let Some((before, after)) = raw.split_once(':') {
                let looks_like_port = after.split(['/', '?', '#']).next().is_some_and(|port| {
                    !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
                });
                if !looks_like_port {
                    return Err(format!(
                        "'{before}:' URLs aren't allowed in a portal (only http and https)"
                    ));
                }
            }
            let host = raw.split(['/', '?', '#']).next().unwrap_or("");
            let scheme = if is_local_host(host_without_port(host)) {
                "http"
            } else {
                "https"
            };
            (scheme.to_string(), raw)
        }
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "'{scheme}:' URLs aren't allowed in a portal (only http and https)"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_without_port(authority.rsplit('@').next().unwrap_or(authority));
    if host.is_empty() {
        return Err(format!("'{raw}' has no host"));
    }
    let rest = if host == "0.0.0.0" {
        rest.replacen("0.0.0.0", "localhost", 1)
    } else {
        rest.to_string()
    };
    Ok(format!("{scheme}://{rest}"))
}

/// `host[:port]` -> `host`, keeping a bracketed IPv6 literal whole.
fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map(|end| &authority[..=end])
            .unwrap_or(authority);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Whether `host` is this machine — what a local dev server prints.
pub fn is_local_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "0.0.0.0"
        || host == "[::1]"
        || host == "[::]"
        || host.starts_with("127.")
            && host.split('.').count() == 4
            && host.split('.').all(|part| part.parse::<u8>().is_ok())
}

/// Checked only when `actor` is `Some` — see this module's doc comment.
pub fn authorize_portal(
    edges: &[EdgeRecord],
    actor: Option<Uuid>,
    portal_id: Uuid,
) -> Result<(), String> {
    match actor {
        Some(actor) => {
            authorize(edges, actor, portal_id, EdgeCapability::ControlPortal).map_err(|_| {
                format!(
                    "not authorized: agent {actor} has no ControlPortal connection to portal \
                     {portal_id} (connect the agent to the portal on the canvas first)"
                )
            })
        }
        None => Ok(()),
    }
}

/// Arbitrary JavaScript evaluation: `ControlPortal` *and* the portal's own
/// `allow_scripts` opt-in for an agent; the human operator is trusted.
pub fn authorize_script(
    edges: &[EdgeRecord],
    actor: Option<Uuid>,
    portal_id: Uuid,
    allow_scripts: bool,
) -> Result<(), String> {
    authorize_portal(edges, actor, portal_id)?;
    if actor.is_some() && !allow_scripts {
        return Err(format!(
            "not authorized: running arbitrary JavaScript in portal {portal_id} is a privileged \
             capability the user hasn't enabled (portal menu → \"Allow agents to run \
             JavaScript\"); use `portal text`/`click`/`type` instead"
        ));
    }
    Ok(())
}

/// Every portal one direct edge away from `agent_id`, whatever (if any)
/// capability that edge grants — discovery, like `notes::notes_connected_to`.
pub fn portals_connected_to(
    nodes: &[NodeRecord],
    edges: &[EdgeRecord],
    agent_id: Uuid,
) -> Vec<Uuid> {
    let portals: BTreeSet<Uuid> = nodes
        .iter()
        .filter(|node| node.as_portal().is_some())
        .map(|node| node.id)
        .collect();
    let mut seen = BTreeSet::new();
    edges
        .iter()
        .filter_map(|edge| {
            let other = if edge.source == agent_id {
                edge.target
            } else if edge.target == agent_id {
                edge.source
            } else {
                return None;
            };
            (portals.contains(&other) && seen.insert(other)).then_some(other)
        })
        .collect()
}

/// A portal name as typed by a user: trimmed, non-empty, no `:` or `@`
/// (they would make `@portal:name` references unparseable) and no
/// whitespace-only names.
pub fn validate_portal_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("a portal needs a name".to_string());
    }
    if name.contains(':') || name.contains('@') {
        return Err("a portal name can't contain ':' or '@'".to_string());
    }
    if name.chars().count() > 60 {
        return Err("a portal name can be at most 60 characters".to_string());
    }
    Ok(name.to_string())
}

/// Where a persistent profile keeps its WebKit data and cache, under
/// `base` (Duet's data directory). `None` for an ephemeral profile, which
/// never touches disk. Derived from the profile id alone, so a profile's
/// storage can never be pointed somewhere else by editing a portal.
pub fn profile_dirs(base: &Path, profile: &PortalProfile) -> Option<(PathBuf, PathBuf)> {
    match profile.storage {
        PortalStorage::Ephemeral => None,
        PortalStorage::Persistent => {
            let root = base.join("portal-profiles").join(profile.id.to_string());
            Some((root.join("data"), root.join("cache")))
        }
    }
}

/// How many screenshots per portal are kept; older ones are deleted when a
/// new one is taken.
pub const SCREENSHOTS_KEPT_PER_PORTAL: usize = 20;

/// The file a new screenshot of `portal_id` is written to: a Duet-chosen
/// name under Duet's own data directory — never a caller-supplied path, so
/// a screenshot request can't be used to write anywhere else.
pub fn screenshot_path(base: &Path, portal_id: Uuid, taken_at_millis: u128) -> PathBuf {
    base.join("portal-screenshots")
        .join(portal_id.to_string())
        .join(format!("{taken_at_millis}.png"))
}

/// Deletes all but the newest `keep` `*.png` screenshots in `dir` (names
/// are millisecond timestamps, so lexical order of equal-length names is
/// age order; sorting by parsed number handles any length). Best effort.
pub fn prune_screenshots(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut shots: Vec<(u128, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stamp = path
                .file_name()?
                .to_str()?
                .strip_suffix(".png")?
                .parse::<u128>()
                .ok()?;
            Some((stamp, path))
        })
        .collect();
    shots.sort();
    let excess = shots.len().saturating_sub(keep);
    for (_, path) in shots.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// How much page text a single `portal text` call returns by default.
pub const DEFAULT_TEXT_LIMIT: usize = 100_000;

/// Truncates `text` to at most `limit` characters, reporting whether it did.
pub fn truncate_text(text: &str, limit: usize) -> (String, bool) {
    match text.char_indices().nth(limit) {
        Some((index, _)) => (text[..index].to_string(), true),
        None => (text.to_string(), false),
    }
}

/// A Rust string as a JavaScript string literal. JSON string syntax is a
/// subset of JS string-literal syntax (ES2019+), and `serde_json` escapes
/// quotes, backslashes and control characters — so user-supplied selectors
/// and text are always data inside these scripts, never code.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}

/// The function body (for `call_async_javascript_function`) that reads a
/// page's URL, title and readable text — or, with `html`, the outer HTML —
/// of the whole document or of the first element matching `selector`.
pub fn text_script(selector: Option<&str>, html: bool) -> String {
    let selector = selector
        .map(js_string)
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"const selector = {selector};
const root = selector === null ? (document.body || document.documentElement) : document.querySelector(selector);
if (!root) return JSON.stringify({{ ok: false, error: "no element matches the selector " + selector }});
const text = {html} ? root.outerHTML : (root.innerText ?? root.textContent ?? "");
return JSON.stringify({{ ok: true, url: location.href, title: document.title, text }});"#,
        html = html,
    )
}

/// Reads where the page is: its URL and title.
pub fn location_script() -> String {
    "return JSON.stringify({ ok: true, url: location.href, title: document.title });".to_string()
}

/// Clicks the first element matching `selector` (scrolled into view and
/// focused first, as a user's click would).
pub fn click_script(selector: &str) -> String {
    format!(
        r#"const selector = {selector};
const el = document.querySelector(selector);
if (!el) return JSON.stringify({{ ok: false, error: "no element matches the selector " + selector }});
el.scrollIntoView({{ block: "center", inline: "center" }});
if (typeof el.focus === "function") el.focus();
el.click();
const label = (el.innerText || el.value || el.getAttribute("aria-label") || "").trim().slice(0, 120);
return JSON.stringify({{ ok: true, detail: "clicked <" + el.tagName.toLowerCase() + ">" + (label ? " \"" + label + "\"" : "") }});"#,
        selector = js_string(selector),
    )
}

/// Types `text` into the first element matching `selector`: replaces its
/// value (or appends, with `append`), fires `input`/`change` the way real
/// typing does (through the native value setter, so framework-controlled
/// inputs such as React's notice), and optionally submits its form.
pub fn type_script(selector: &str, text: &str, append: bool, submit: bool) -> String {
    format!(
        r#"const selector = {selector};
const text = {text};
const el = document.querySelector(selector);
if (!el) return JSON.stringify({{ ok: false, error: "no element matches the selector " + selector }});
el.scrollIntoView({{ block: "center", inline: "center" }});
if (typeof el.focus === "function") el.focus();
if (el.isContentEditable) {{
  el.textContent = ({append} ? el.textContent : "") + text;
  el.dispatchEvent(new InputEvent("input", {{ bubbles: true, data: text, inputType: "insertText" }}));
}} else if ("value" in el && !(el instanceof HTMLButtonElement)) {{
  const next = ({append} ? String(el.value) : "") + text;
  let proto = Object.getPrototypeOf(el), setter = null;
  while (proto && !setter) {{
    const desc = Object.getOwnPropertyDescriptor(proto, "value");
    setter = desc && desc.set;
    proto = Object.getPrototypeOf(proto);
  }}
  if (setter) setter.call(el, next); else el.value = next;
  el.dispatchEvent(new InputEvent("input", {{ bubbles: true, data: text, inputType: "insertText" }}));
  el.dispatchEvent(new Event("change", {{ bubbles: true }}));
}} else {{
  return JSON.stringify({{ ok: false, error: "the element matching " + selector + " is not editable" }});
}}
if ({submit}) {{
  if (el.form) {{
    if (typeof el.form.requestSubmit === "function") el.form.requestSubmit(); else el.form.submit();
  }} else {{
    for (const type of ["keydown", "keypress", "keyup"])
      el.dispatchEvent(new KeyboardEvent(type, {{ key: "Enter", code: "Enter", keyCode: 13, bubbles: true }}));
  }}
}}
return JSON.stringify({{ ok: true, detail: "typed " + text.length + " characters into <" + el.tagName.toLowerCase() + ">" + ({submit} ? " and submitted" : "") }});"#,
        selector = js_string(selector),
        text = js_string(text),
        append = append,
        submit = submit,
    )
}

/// How `portal evaluate` runs a caller's arbitrary script: first as one
/// expression (`document.title`, `fetch("/api").then(r => r.status)` —
/// its value, awaited if it's a promise, is the result), and only if that
/// doesn't even *parse* as an expression, as a block of statements
/// (`const a = 1; return a + 1;` — `return` gives the result). The script
/// is spliced into the injected function rather than passed to `eval`, so
/// a page's Content-Security-Policy (no `unsafe-eval`) can't block it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluateForm {
    Expression,
    Statements,
}

/// The function body for evaluating `script` in `form`. Its own
/// exceptions come back in the JSON envelope; a script that fails to parse
/// makes the whole call fail instead (no part of it ran), which is the one
/// case [`is_parse_failure`] lets the caller retry as statements.
pub fn evaluate_script(script: &str, form: EvaluateForm) -> String {
    let run = match form {
        EvaluateForm::Expression => format!(
            "await (\n{}\n)",
            script.trim().trim_end_matches(';').trim_end()
        ),
        EvaluateForm::Statements => format!("await (async () => {{\n{script}\n}})()"),
    };
    format!(
        r#"let duetValue;
try {{
  duetValue = {run};
}} catch (error) {{
  return JSON.stringify({{ ok: false, error: String(error) }});
}}
let duetJson;
try {{ duetJson = JSON.stringify(duetValue === undefined ? null : duetValue); }} catch (error) {{ duetJson = JSON.stringify(String(duetValue)); }}
return JSON.stringify({{ ok: true, value: duetJson === undefined ? null : JSON.parse(duetJson) }});"#
    )
}

/// Whether a failed `evaluate` call failed because the script didn't parse
/// in the form it was tried in (so nothing ran and retrying in the other
/// form is safe), rather than because it ran and threw.
pub fn is_parse_failure(error: &str) -> bool {
    error.contains("SyntaxError")
}

/// The JSON envelope every Duet page script returns.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScriptReply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

/// Parses a page script's raw string result into its envelope, turning a
/// script-reported failure (`ok: false`) into an `Err`.
pub fn parse_script_reply(raw: &str) -> Result<ScriptReply, String> {
    let reply: ScriptReply = serde_json::from_str(raw)
        .map_err(|error| format!("the page returned an unexpected result ({error})"))?;
    if reply.ok {
        Ok(reply)
    } else {
        Err(reply
            .error
            .unwrap_or_else(|| "the page script failed".to_string()))
    }
}

/// Finds local development-server URLs (`http://localhost:3000`,
/// `http://127.0.0.1:5173/`, `http://0.0.0.0:8000` -> `localhost`, ...) in
/// terminal output. ANSI escape sequences are stripped first: Vite, for
/// one, prints the port in bold *inside* the URL. Only loopback hosts count
/// — a remote URL in a log line is not a dev server Duet should offer to
/// open. Results are normalized and deduplicated in first-seen order.
pub fn detect_dev_server_urls(output: &str) -> Vec<String> {
    let text = strip_ansi(output);
    let mut found: Vec<String> = Vec::new();
    for (start, _) in text.match_indices("http") {
        let candidate = &text[start..];
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            continue;
        }
        if start > 0
            && text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        let end = candidate
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|' | ')' | ']')
            })
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ',', ';', ':', '!']);
        let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
        let host = host_without_port(authority);
        if !is_local_host(host) {
            continue;
        }
        let port = authority.strip_prefix(host).unwrap_or("");
        if !(port.is_empty()
            || port.starts_with(':')
                && port[1..].chars().all(|c| c.is_ascii_digit())
                && port.len() > 1)
        {
            continue;
        }
        if let Ok(normalized) = normalize_url(url)
            && !found.contains(&normalized)
        {
            found.push(normalized);
        }
    }
    found
}

/// Removes ANSI CSI (`ESC [ ... final`) and OSC (`ESC ] ... BEL|ST`)
/// sequences, plus any other lone `ESC x` pair.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Per-terminal dev-server URL detection state: buffers a partial last line
/// (a URL can arrive split across two PTY reads) and remembers what was
/// already offered, so each URL is offered once per terminal. Runtime-only.
#[derive(Debug, Default)]
pub struct DevUrlScanner {
    partial: String,
    seen: BTreeSet<String>,
    /// Every URL this terminal has printed, oldest first — what the card
    /// menu's "Open in Portal" lists.
    pub detected: Vec<String>,
}

/// A partial line longer than this is scanned as-is and dropped, so a
/// process that never prints a newline can't grow the buffer forever.
const MAX_PARTIAL_LINE: usize = 4096;

impl DevUrlScanner {
    /// Feeds raw terminal output; returns URLs seen for the first time.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        let complete = match self.partial.rfind(['\n', '\r']) {
            Some(index) => {
                let rest = self.partial.split_off(index + 1);
                std::mem::replace(&mut self.partial, rest)
            }
            None if self.partial.len() > MAX_PARTIAL_LINE => std::mem::take(&mut self.partial),
            None => return Vec::new(),
        };
        let mut new = Vec::new();
        for url in detect_dev_server_urls(&complete) {
            if self.seen.insert(url.clone()) {
                self.detected.push(url.clone());
                new.push(url);
            }
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FloorRef, NodeKind, PortalPayload};

    fn edge(source: Uuid, target: Uuid, capabilities: &[EdgeCapability]) -> EdgeRecord {
        EdgeRecord {
            id: Uuid::new_v4(),
            source,
            target,
            capabilities: capabilities.iter().copied().collect(),
        }
    }

    #[test]
    fn urls_are_normalized_and_dangerous_schemes_refused() {
        for (raw, expected) in [
            ("localhost:3000", "http://localhost:3000"),
            ("127.0.0.1:5173/login", "http://127.0.0.1:5173/login"),
            ("http://0.0.0.0:8000/x", "http://localhost:8000/x"),
            ("example.com", "https://example.com"),
            ("  https://docs.rs/webkit6  ", "https://docs.rs/webkit6"),
            ("HTTP://localhost", "http://localhost"),
            ("about:blank", "about:blank"),
            ("app.localhost:8080", "http://app.localhost:8080"),
        ] {
            assert_eq!(normalize_url(raw).as_deref(), Ok(expected), "{raw}");
        }
        for raw in [
            "",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
            "ftp://x",
            "http://",
            "a b",
            "about:config",
        ] {
            assert!(normalize_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn control_portal_is_required_for_agents_and_scripts_need_the_opt_in() {
        let (agent, portal, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![
            edge(agent, portal, &[EdgeCapability::ControlPortal]),
            edge(other, portal, &[]),
        ];
        assert!(authorize_portal(&edges, Some(agent), portal).is_ok());
        // Visual-only edge, no edge, and a different capability: refused.
        assert!(authorize_portal(&edges, Some(other), portal).is_err());
        assert!(authorize_portal(&[], Some(agent), portal).is_err());
        let notes_only = vec![edge(agent, portal, &[EdgeCapability::ReadNote])];
        assert!(authorize_portal(&notes_only, Some(agent), portal).is_err());
        // The human operator is trusted.
        assert!(authorize_portal(&[], None, portal).is_ok());

        assert!(authorize_script(&edges, Some(agent), portal, false).is_err());
        assert!(authorize_script(&edges, Some(agent), portal, true).is_ok());
        assert!(authorize_script(&edges, Some(other), portal, true).is_err());
        assert!(authorize_script(&[], None, portal, false).is_ok());
    }

    fn portal_node(name: &str) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Portal(PortalPayload::new(name, "")),
        }
    }

    #[test]
    fn discovery_is_one_hop_and_deduplicated() {
        let agent = Uuid::new_v4();
        let (a, b, c) = (portal_node("A"), portal_node("B"), portal_node("C"));
        let edges = vec![
            edge(agent, a.id, &[EdgeCapability::ControlPortal]),
            edge(b.id, agent, &[]),
            edge(agent, a.id, &[]),
            edge(b.id, c.id, &[]),
        ];
        let nodes = vec![a.clone(), b.clone(), c];
        assert_eq!(
            portals_connected_to(&nodes, &edges, agent),
            vec![a.id, b.id]
        );
    }

    #[test]
    fn portal_names_are_validated() {
        assert_eq!(
            validate_portal_name("  Frontend "),
            Ok("Frontend".to_string())
        );
        assert!(validate_portal_name("   ").is_err());
        assert!(validate_portal_name("portal:x").is_err());
        assert!(validate_portal_name("@x").is_err());
    }

    #[test]
    fn profiles_get_separate_directories_and_ephemeral_ones_none() {
        let base = Path::new("/data/duet");
        let a = PortalProfile::isolated();
        let b = PortalProfile::isolated();
        let (a_data, a_cache) = profile_dirs(base, &a).unwrap();
        let (b_data, _) = profile_dirs(base, &b).unwrap();
        assert_ne!(a_data, b_data);
        assert!(a_data.starts_with(base.join("portal-profiles")));
        assert!(a_cache.ends_with("cache"));
        let ephemeral = PortalProfile {
            id: Uuid::new_v4(),
            storage: PortalStorage::Ephemeral,
        };
        assert_eq!(profile_dirs(base, &ephemeral), None);
    }

    #[test]
    fn screenshots_live_under_duet_data_and_are_pruned_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let portal = Uuid::new_v4();
        let path = screenshot_path(tmp.path(), portal, 42);
        assert!(path.starts_with(tmp.path().join("portal-screenshots")));
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        for stamp in [5u128, 100, 9, 1000, 20] {
            std::fs::write(dir.join(format!("{stamp}.png")), b"x").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"left alone").unwrap();
        prune_screenshots(dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, vec!["100.png", "1000.png", "notes.txt"]);
    }

    #[test]
    fn text_truncation_counts_characters() {
        assert_eq!(truncate_text("héllo", 3), ("hél".to_string(), true));
        assert_eq!(truncate_text("hi", 3), ("hi".to_string(), false));
    }

    #[test]
    fn scripts_embed_arguments_as_data_not_code() {
        let nasty = r#"a"); alert("x"); ("#;
        let script = click_script(nasty);
        assert!(script.contains(r#"const selector = "a\"); alert(\"x\"); (";"#));
        let typed = type_script("#q", "line1\nline2 \"quoted\" \\ </script>", false, true);
        assert!(typed.contains(r#"const text = "line1\nline2 \"quoted\" \\ </script>";"#));
        let text = text_script(None, false);
        assert!(text.contains("const selector = null;"));
        let expression = evaluate_script("document.title;  ", EvaluateForm::Expression);
        assert!(expression.contains("duetValue = await (\ndocument.title\n);"));
        let statements = evaluate_script("const a = 1; return a;", EvaluateForm::Statements);
        assert!(statements.contains("await (async () => {\nconst a = 1; return a;\n})()"));
        assert!(is_parse_failure(
            "the page script failed: SyntaxError: Unexpected token ';'"
        ));
        assert!(!is_parse_failure(
            "the page script failed: TypeError: x is undefined"
        ));
    }

    #[test]
    fn script_replies_parse_and_failures_become_errors() {
        let ok =
            parse_script_reply(r#"{"ok":true,"url":"http://x/","title":"T","text":"hi"}"#).unwrap();
        assert_eq!(ok.text.as_deref(), Some("hi"));
        assert_eq!(
            parse_script_reply(r#"{"ok":false,"error":"no element"}"#),
            Err("no element".to_string())
        );
        assert!(parse_script_reply("not json").is_err());
        let value = parse_script_reply(r#"{"ok":true,"value":{"a":[1,2]}}"#).unwrap();
        assert_eq!(value.value, Some(serde_json::json!({"a": [1, 2]})));
    }

    #[test]
    fn dev_server_urls_are_detected_through_ansi_noise() {
        let vite = "\u{1b}[32m  ➜\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m:   \u{1b}[36mhttp://localhost:\u{1b}[1m5173\u{1b}[22m/\u{1b}[39m\n  ➜  Network: use --host to expose\n";
        assert_eq!(detect_dev_server_urls(vite), vec!["http://localhost:5173/"]);
        let mixed = "Listening on http://0.0.0.0:8000 (press CTRL+C)\n\
                     see https://example.com/docs and http://127.0.0.1:3000/app.\n\
                     ready at http://localhost:3000, http://localhost:3000\n\
                     not a url: xhttp://localhost:1 or http://localhost:abc\n";
        assert_eq!(
            detect_dev_server_urls(mixed),
            vec![
                "http://localhost:8000",
                "http://127.0.0.1:3000/app",
                "http://localhost:3000"
            ]
        );
        assert!(detect_dev_server_urls("no urls here").is_empty());
    }

    #[test]
    fn scanner_handles_split_reads_and_offers_each_url_once() {
        let mut scanner = DevUrlScanner::default();
        assert!(scanner.feed(b"Server running at http://local").is_empty());
        assert_eq!(
            scanner.feed(b"host:3000/\r\n"),
            vec!["http://localhost:3000/"]
        );
        assert!(scanner.feed(b"again http://localhost:3000/\n").is_empty());
        assert_eq!(
            scanner.feed(b"admin http://127.0.0.1:4000\n"),
            vec!["http://127.0.0.1:4000"]
        );
        assert_eq!(
            scanner.detected,
            vec!["http://localhost:3000/", "http://127.0.0.1:4000"]
        );
        let long = vec![b'x'; MAX_PARTIAL_LINE + 10];
        scanner.feed(&long);
        assert!(scanner.partial.is_empty());
    }
}
