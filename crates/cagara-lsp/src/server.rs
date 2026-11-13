//! The language server over stdio (`cagara lsp`). Full-text sync; each open
//! document is the root of its own workspace, updated with `set_source`
//! (reloaded when its imports change). Imported files are read from disk.

use crate::{analysis, uri};
use cagara_hir::workspace::Workspace;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
    DidSaveTextDocument, LogMessage, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, Formatting,
    GotoDefinition, HoverRequest, InlayHintRequest, References, Request as _,
};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CompletionOptions, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, LogMessageParams, MarkupContent,
    MarkupKind, MessageType, OneOf, PublishDiagnosticsParams, ReferenceParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri, WorkspaceEdit,
};
use std::collections::HashMap;
use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct Doc {
    path: PathBuf,
    text: String,
    /// The `version` the client last sent. It goes back out with the
    /// diagnostics, so a client can tell a stale publish from a current one
    /// when they arrive out of order.
    version: Option<i32>,
}

struct State {
    /// Open documents, keyed by their URI string (`Uri` has interior
    /// mutability, so it is not a key itself).
    docs: HashMap<String, Doc>,
    /// All open buffers share one module graph. The selected root is changed
    /// for each request so imported unsaved files are visible everywhere.
    ws: Workspace,
}

impl State {
    fn new() -> Self {
        Self {
            docs: HashMap::new(),
            ws: Workspace::from_source(""),
        }
    }

    fn buffers(&self) -> HashMap<PathBuf, String> {
        self.docs
            .values()
            .map(|d| (d.path.clone(), d.text.clone()))
            .collect()
    }

    fn rebuild(&mut self) {
        let Some((path, text)) = self
            .docs
            .values()
            .next()
            .map(|d| (d.path.clone(), d.text.clone()))
        else {
            self.ws = Workspace::from_source("");
            return;
        };
        let before = self.ws.texts_by_path();
        let buffers = self.buffers();
        self.ws = Workspace::open_with_buffers(&path, text, &buffers);
        // A rebuild marks every module as new; what matters is which documents
        // it made a *different* answer for. The graph is built from the same
        // buffers, so unless a file on disk changed under it, that is none of
        // them — and publishing all N documents to find that out is how a
        // rebuild turned into a minute of work.
        self.ws.take_needs_publish();
        let changed: Vec<bool> = self
            .ws
            .modules
            .iter()
            .map(|m| before.get(&m.path) != Some(&m.text))
            .collect();
        let roots: Vec<usize> = self
            .docs
            .values()
            .filter_map(|doc| self.ws.module_for_path(&doc.path))
            .collect();
        for root in roots {
            let reach = self.ws.modules_reachable_from(root);
            if changed
                .iter()
                .zip(&reach)
                .any(|(changed, reached)| *changed && *reached)
            {
                self.ws.mark_needs_publish(root);
            }
        }
    }

    fn select(&mut self, key: &str) -> bool {
        self.docs
            .get(key)
            .is_some_and(|d| self.ws.set_root_path(&d.path))
    }
}

/// Why a request got no result.
type Fail = (ErrorCode, String);

fn bad_params(e: serde_json::Error) -> Fail {
    (ErrorCode::InvalidParams, e.to_string())
}

fn json(v: serde_json::Result<serde_json::Value>) -> Result<serde_json::Value, Fail> {
    v.map_err(|e| (ErrorCode::InternalError, e.to_string()))
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    match (p.downcast_ref::<&str>(), p.downcast_ref::<String>()) {
        (Some(s), _) => s.to_string(),
        (_, Some(s)) => s.clone(),
        _ => "unknown panic".into(),
    }
}

/// Serve LSP over stdin / stdout until the client shuts the server down.
/// A malformed message gets an error reply (or is logged, for a
/// notification), and a panic while handling one is reported the same way,
/// so neither ends the session.
///
/// `build` is what the binary says it is — version, commit, profile, target,
/// build time — and is announced in the client's log before anything else. It
/// is passed in because only `cagara-cli` can stamp its own build; this crate
/// is a library.
pub fn run(build: &str) -> Res<()> {
    let (conn, io) = Connection::stdio();
    serve(&conn, build)?;
    // The writer thread ends when every sender is gone.
    drop(conn);
    io.join()?;
    Ok(())
}

/// Initialize, then handle messages until shutdown or disconnect.
///
/// Two threads: this one only reads messages and routes them, and an analysis
/// thread owns the workspace and does the work. That split is what keeps
/// typing from queueing: while a document is being checked, further keystrokes
/// arrive and pile up here instead of waiting to be read, and [`next_batch`]
/// then drops the ones a later keystroke has already overwritten. Analysing a
/// burst of ten keystrokes used to cost ten checks of the file; it now costs
/// one, of the text the user is actually looking at.
fn serve(conn: &Connection, build: &str) -> Res<()> {
    let caps = ServerCapabilities {
        // Incremental: the client sends the range it changed, not the whole
        // document. A 2000-line file costs a few dozen bytes per keystroke
        // instead of the file, and the server applies the edit to the text it
        // already has — see `apply_change`.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        // The inferred type of a definition that does not write one, at the end
        // of its name (`analysis::type_hints`).
        inlay_hint_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    conn.initialize(serde_json::to_value(caps)?)?;
    // The client logs this next to its own output, so the answer to "which
    // compiler is this window running" is in the log rather than in a guess.
    // A copy carries the copy's timestamp and a staged file looks new, so the
    // binary has to name itself.
    conn.sender.send(Message::Notification(Notification::new(
        LogMessage::METHOD.to_string(),
        serde_json::to_value(LogMessageParams {
            typ: MessageType::INFO,
            message: format!("cagara {build}"),
        })?,
    )))?;
    let (work, jobs) = crossbeam_channel::unbounded::<Work>();
    let sender = conn.sender.clone();
    let worker = std::thread::Builder::new()
        .name("cagara-analysis".into())
        .spawn(move || analyse(jobs, sender))?;
    // A change is published for its own document immediately, so the file
    // being edited shows its diagnostics without waiting. Other open documents
    // may import it, so theirs are refreshed once the typing pauses — and only
    // those: the publish that follows a burst is the set of roots the workspace
    // invalidated, not every document that happens to be open.
    let debounce = Duration::from_millis(150);
    let mut publish_at: Option<Instant> = None;
    loop {
        let timeout = publish_at
            .map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        let msg = match conn.receiver.recv_timeout(timeout) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                publish_at = None;
                let _ = work.send(Work::Publish);
                continue;
            }
            // Disconnected; the writer thread will end with it.
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    break;
                }
                let Request { id, method, params } = req;
                let (reply, answer) = mpsc::channel();
                if work
                    .send(Work::Request {
                        method: method.clone(),
                        params,
                        reply,
                    })
                    .is_err()
                {
                    break;
                }
                let resp = match answer.recv() {
                    Ok(Ok(v)) => Response::new_ok(id, v),
                    Ok(Err((code, msg))) => Response::new_err(id, code as i32, msg),
                    Err(_) => Response::new_err(
                        id,
                        ErrorCode::InternalError as i32,
                        format!("internal error in {method}: analysis stopped"),
                    ),
                };
                conn.sender.send(Message::Response(resp))?;
            }
            Message::Notification(n) => {
                // These change the text the other documents are checked
                // against, so the documents whose modules moved have their
                // diagnostics refreshed once the stream goes quiet. Which ones
                // moved is the workspace's answer (`Workspace::take_needs_publish`),
                // not a guess made here.
                let refresh = matches!(
                    n.method.as_str(),
                    DidOpenTextDocument::METHOD
                        | DidChangeTextDocument::METHOD
                        | DidSaveTextDocument::METHOD
                        | DidChangeWatchedFiles::METHOD
                        | DidCloseTextDocument::METHOD
                );
                if work.send(Work::Notify(n)).is_err() {
                    break;
                }
                if refresh {
                    publish_at = Some(Instant::now() + debounce);
                }
            }
            Message::Response(_) => {}
        }
    }
    // The analysis thread ends when its channel closes.
    drop(work);
    let _ = worker.join();
    Ok(())
}

/// Work for the analysis thread.
enum Work {
    Request {
        method: String,
        params: serde_json::Value,
        reply: mpsc::Sender<Result<serde_json::Value, Fail>>,
    },
    Notify(Notification),
    /// Refresh the diagnostics of every open document.
    Publish,
}

/// Everything already queued, with superseded document updates dropped.
struct Batch {
    notify: Vec<Notification>,
    requests: Vec<Work>,
    publish: bool,
}

/// The analysis thread: own the workspace, do the work, never read stdin.
fn analyse(jobs: Receiver<Work>, sender: Sender<Message>) {
    let mut state = State::new();
    while let Some(batch) = next_batch(&jobs) {
        for n in batch.notify {
            let method = n.method.clone();
            match catch_unwind(AssertUnwindSafe(|| notification(&sender, &mut state, n))) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("cagara lsp: {method}: {e}"),
                Err(p) => {
                    rebuild(&mut state);
                    eprintln!(
                        "cagara lsp: internal error in {method}: {}",
                        panic_message(&*p)
                    );
                }
            }
        }
        for job in batch.requests {
            let Work::Request {
                method,
                params,
                reply,
            } = job
            else {
                unreachable!("only requests are collected here");
            };
            let answer =
                match catch_unwind(AssertUnwindSafe(|| request(&mut state, &method, params))) {
                    Ok(result) => result,
                    Err(p) => {
                        rebuild(&mut state);
                        Err((
                            ErrorCode::InternalError,
                            format!("internal error in {method}: {}", panic_message(&*p)),
                        ))
                    }
                };
            let _ = reply.send(answer);
        }
        if batch.publish {
            if let Err(e) = publish_dirty(&sender, &mut state) {
                eprintln!("cagara lsp: publish: {e}");
            }
        }
    }
}

/// Wait for one message, then take everything else already queued.
///
/// Requests are all kept — each needs its own answer — but only the newest
/// text of a document matters, so earlier `didChange`s for the same document
/// are dropped: their text no longer exists anywhere, and checking it would
/// only delay the text that does.
fn next_batch(jobs: &Receiver<Work>) -> Option<Batch> {
    let first = jobs.recv().ok()?;
    let mut batch = Batch {
        notify: Vec::new(),
        requests: Vec::new(),
        publish: false,
    };
    for work in std::iter::once(first).chain(jobs.try_iter()) {
        match work {
            Work::Notify(n) => batch.notify.push(n),
            Work::Publish => batch.publish = true,
            request @ Work::Request { .. } => batch.requests.push(request),
        }
    }
    drop_superseded_changes(&mut batch.notify);
    Some(batch)
}

/// Keep only the last `didChange` for each document.
fn drop_superseded_changes(notifications: &mut Vec<Notification>) {
    if notifications.len() < 2 {
        return;
    }
    let uri_of = |n: &Notification| -> Option<String> {
        if n.method != DidChangeTextDocument::METHOD {
            return None;
        }
        n.params
            .pointer("/textDocument/uri")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let mut last: HashMap<String, usize> = HashMap::new();
    for (i, n) in notifications.iter().enumerate() {
        if let Some(uri) = uri_of(n) {
            last.insert(uri, i);
        }
    }
    let mut kept = Vec::with_capacity(notifications.len());
    for (i, n) in notifications.drain(..).enumerate() {
        let superseded = uri_of(&n).is_some_and(|uri| last.get(&uri) != Some(&i));
        if !superseded {
            kept.push(n);
        }
    }
    *notifications = kept;
}

/// A panic may have left a workspace half-updated (completion edits the
/// text in place); start each one over from what the client last sent.
fn rebuild(state: &mut State) {
    let _ = catch_unwind(AssertUnwindSafe(|| state.rebuild()));
}

/// The key a document is stored under.
///
/// `didOpen` inserts with this and every request selects with it. Building the
/// key in one place is what keeps the two conventions identical: a mismatch
/// yields an empty result rather than an error.
fn doc_key(uri: &Uri) -> String {
    uri.as_str().to_string()
}

/// Apply one LSP range edit to `text`.
///
/// Positions are UTF-16 line/character pairs, which is what the protocol
/// counts and not what Rust strings count: a column is a number of `u16` code
/// units, so a character outside the basic plane counts as two. The conversion
/// is done here rather than by editing bytes, because a byte offset computed
/// from a UTF-16 column lands inside a character and panics on the next slice.
///
/// A range that does not fit the text — a client and server that disagree
/// about the version — replaces nothing rather than panicking: the next
/// `didOpen` (or a full-document change) resynchronises, and an editor must not
/// be able to end the session with a stale offset.
fn apply_change(text: &str, range: lsp_types::Range, replacement: &str) -> String {
    let offset = |p: lsp_types::Position| -> Option<usize> {
        let line_start = text
            .split('\n')
            .take(p.line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>();
        if p.line as usize > text.split('\n').count() {
            return None;
        }
        let line = text.split('\n').nth(p.line as usize)?;
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            if units == p.character {
                return Some(line_start + i);
            }
            units += c.len_utf16() as u32;
            if units > p.character {
                // The position points inside a character.
                return None;
            }
        }
        (units == p.character).then_some(line_start + line.len())
    };
    match (offset(range.start), offset(range.end)) {
        (Some(a), Some(b)) if a <= b && b <= text.len() => {
            let mut out = String::with_capacity(text.len() - (b - a) + replacement.len());
            out.push_str(&text[..a]);
            out.push_str(replacement);
            out.push_str(&text[b..]);
            out
        }
        _ => text.to_string(),
    }
}

fn request(
    state: &mut State,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Fail> {
    match method {
        HoverRequest::METHOD => {
            let p: HoverParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position_params;
            let key = doc_key(&tp.text_document.uri);
            let h = state
                .select(&key)
                .then(|| analysis::hover(&state.ws, tp.position))
                .flatten();
            json(serde_json::to_value(h.map(|value| Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: None,
            })))
        }
        GotoDefinition::METHOD => {
            let p: GotoDefinitionParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position_params;
            let key = doc_key(&tp.text_document.uri);
            let locs: Vec<Location> = if state.select(&key) {
                analysis::definition(&state.ws, tp.position)
            } else {
                vec![]
            }
            .into_iter()
            .filter_map(|(path, range)| {
                Some(Location {
                    uri: uri::from_path(&path)?,
                    range,
                })
            })
            .collect();
            json(serde_json::to_value(
                (!locs.is_empty()).then_some(GotoDefinitionResponse::Array(locs)),
            ))
        }
        References::METHOD => {
            let p: ReferenceParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position;
            let key = doc_key(&tp.text_document.uri);
            let uri = tp.text_document.uri;
            let locs: Option<Vec<Location>> = state.select(&key).then(|| {
                analysis::references(&state.ws, tp.position, p.context.include_declaration)
                    .into_iter()
                    .map(|range| Location {
                        uri: uri.clone(),
                        range,
                    })
                    .collect()
            });
            json(serde_json::to_value(locs))
        }
        DocumentHighlightRequest::METHOD => {
            let p: DocumentHighlightParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position_params;
            let key = doc_key(&tp.text_document.uri);
            let hs: Option<Vec<DocumentHighlight>> = state.select(&key).then(|| {
                analysis::highlights(&state.ws, tp.position)
                    .into_iter()
                    .map(|(range, kind)| DocumentHighlight {
                        range,
                        kind: Some(kind),
                    })
                    .collect()
            });
            json(serde_json::to_value(hs))
        }
        DocumentSymbolRequest::METHOD => {
            let p: DocumentSymbolParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let ss = state.select(&key).then(|| analysis::symbols(&state.ws));
            json(serde_json::to_value(ss.map(DocumentSymbolResponse::Nested)))
        }
        Completion::METHOD => {
            let p: CompletionParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position;
            let key = doc_key(&tp.text_document.uri);
            let items = state
                .select(&key)
                .then(|| analysis::completion(&mut state.ws, tp.position));
            json(serde_json::to_value(items.map(CompletionResponse::Array)))
        }
        CodeActionRequest::METHOD => {
            let p: CodeActionParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let uri = p.text_document.uri;
            let fixes = if state.select(&key) {
                analysis::code_actions(&state.ws, p.range)
            } else {
                Vec::new()
            };
            let actions: Vec<CodeActionOrCommand> = fixes
                .into_iter()
                .map(|f| {
                    CodeActionOrCommand::CodeAction(CodeAction {
                        title: f.title,
                        kind: Some(CodeActionKind::QUICKFIX),
                        edit: Some(WorkspaceEdit {
                            changes: Some(HashMap::from([(uri.clone(), f.edits)])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                })
                .collect();
            json(serde_json::to_value(actions))
        }
        InlayHintRequest::METHOD => {
            let p: InlayHintParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let hints: Vec<InlayHint> = match state.select(&key) {
                false => Vec::new(),
                true => analysis::type_hints(&state.ws)
                    .into_iter()
                    .filter(|(r, _)| range_contains(&p.range, r.start))
                    .map(|(r, label)| InlayHint {
                        position: r.start,
                        label: InlayHintLabel::String(label),
                        kind: Some(InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: Some(true),
                        padding_right: None,
                        data: None,
                    })
                    .collect(),
            };
            json(serde_json::to_value(hints))
        }
        Formatting::METHOD => {
            let p: DocumentFormattingParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            match state.select(&key) {
                false => Ok(serde_json::Value::Null),
                true => match analysis::format(&state.ws) {
                    Ok(edits) => json(serde_json::to_value(edits)),
                    Err(e) => Err((ErrorCode::InternalError, e.to_string())),
                },
            }
        }
        _ => Err((ErrorCode::MethodNotFound, format!("unsupported: {method}"))),
    }
}

/// Is `pos` inside `range`? LSP ranges are half-open, and a hint sits exactly at
/// a definition's name end, so the end counts as inside for a requested range
/// that stops there (the editor asks with the visible viewport).
fn range_contains(range: &lsp_types::Range, pos: lsp_types::Position) -> bool {
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    before_end && after_start
}

fn notification(sender: &Sender<Message>, state: &mut State, n: Notification) -> Res<()> {
    match n.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let p: lsp_types::DidOpenTextDocumentParams = serde_json::from_value(n.params)?;
            let Some(path) = uri::to_path(&p.text_document.uri) else {
                return Ok(());
            };
            let path = path.canonicalize().unwrap_or(path);
            let text = p.text_document.text;
            let key = doc_key(&p.text_document.uri);
            state.docs.insert(
                key.clone(),
                Doc {
                    path: path.clone(),
                    text: text.clone(),
                    version: Some(p.text_document.version),
                },
            );
            // Opening a document adds it to the graph that is already loaded.
            // Rebuilding the graph for it threw away every other document's
            // parse and check, so opening the Nth tab cost the checks of all N
            // (see `Workspace::add_buffer`).
            if !state.ws.add_buffer(&path, text) {
                state.rebuild();
            }
            publish_doc(sender, state, &key)?;
            Ok(())
        }
        DidChangeTextDocument::METHOD => {
            let p: lsp_types::DidChangeTextDocumentParams = serde_json::from_value(n.params)?;
            let key = doc_key(&p.text_document.uri);
            let Some(doc) = state.docs.get_mut(&key) else {
                return Ok(());
            };
            doc.version = Some(p.text_document.version);
            // The client sends each change against the text *it* has already
            // applied, so they are applied in order to the text we have. A
            // change with no range is a whole-document replacement, which is
            // what a client that ignores the incremental capability sends.
            for change in p.content_changes {
                doc.text = match change.range {
                    None => change.text,
                    Some(range) => apply_change(&doc.text, range, &change.text),
                };
            }
            // A keystroke usually does not change imports: patch that module
            // in place so salsa's memoized checks survive, and rebuild the
            // whole graph from disk only when they do.
            let text = doc.text.clone();
            let patched = state
                .ws
                .module_for_path(&doc.path)
                .is_some_and(|m| state.ws.set_source(m, text));
            if !patched {
                state.rebuild();
            }
            // The edited document's diagnostics are what the user is looking
            // at; publish them now. The other open documents are refreshed
            // once the typing pauses (see `serve`).
            publish_doc(sender, state, &key)?;
            Ok(())
        }
        DidSaveTextDocument::METHOD => {
            let p: lsp_types::DidSaveTextDocumentParams = serde_json::from_value(n.params)?;
            if let (Some(doc), Some(text)) =
                (state.docs.get_mut(p.text_document.uri.as_str()), p.text)
            {
                doc.text = text;
            }
            // A save may have changed an imported file that is not open.
            // Re-read those files in place, which keeps the salsa memos and
            // every open document's compilation; only an import that changed
            // needs the graph rebuilt around it.
            //
            // Nothing is published here. `reload_from_disk` marks the roots
            // whose modules actually changed — usually none, because the
            // document that was just saved is a buffer and the files it
            // imports did not move — and the refresh `serve` schedules behind a
            // burst publishes those. Publishing every open document from here
            // as well meant a save recompiled each of N documents twice more
            // than the one document that changed.
            if !state.ws.reload_from_disk() {
                state.rebuild();
            }
            Ok(())
        }
        DidChangeWatchedFiles::METHOD => {
            let _: lsp_types::DidChangeWatchedFilesParams = serde_json::from_value(n.params)?;
            // Same as a save, and for the same reason: a watcher event is one
            // file changing, and the roots that read it are the ones to
            // publish.
            if !state.ws.reload_from_disk() {
                state.rebuild();
            }
            Ok(())
        }
        DidCloseTextDocument::METHOD => {
            let p: lsp_types::DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
            state.docs.remove(p.text_document.uri.as_str());
            // The document is no longer an overlay: its file, if it is on
            // disk, is now what the graph should use. Only that module is
            // re-read; the rest of the graph, and every other document's
            // compilation, stays memoized.
            let closed =
                uri::to_path(&p.text_document.uri).map(|path| path.canonicalize().unwrap_or(path));
            if let Some(path) = &closed {
                if let Some(m) = state.ws.module_for_path(path) {
                    // Nothing will ask about this root again: drop both of its
                    // compilations rather than keep them for a document that
                    // is closed.
                    state.ws.forget_root(m);
                }
                if !state.ws.drop_buffer(path) {
                    state.rebuild();
                }
            }
            // The client keeps markers for a document it still has; clearing
            // them is the other half of closing it.
            let params = PublishDiagnosticsParams {
                uri: p.text_document.uri,
                diagnostics: vec![],
                version: None,
            };
            sender.send(Message::Notification(Notification::new(
                PublishDiagnostics::METHOD.into(),
                params,
            )))?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Publish diagnostics for one open document, making it the workspace root.
fn publish_doc(sender: &Sender<Message>, state: &mut State, key: &str) -> Res<()> {
    if !state.select(key) {
        return Ok(());
    }
    let uri: Uri = key
        .parse()
        .map_err(|e| format!("invalid document URI: {e}"))?;
    let version = state.docs.get(key).and_then(|d| d.version);
    publish(sender, uri, &state.ws, version)?;
    // These diagnostics were computed from the workspace as it is now, so this
    // root has nothing outstanding.
    let root = state.ws.root;
    state.ws.published(root);
    Ok(())
}

/// Publish the diagnostics that actually changed, and nothing else.
///
/// The workspace records which roots it has invalidated (`needs_publish`), and
/// that set is what an edit, a save, or a watched-file event leaves behind:
/// the documents that read the module that moved. Everything else was
/// published against a compilation that is still exactly right, so
/// recomputing it would be work with no result — and on this project, where a
/// document's imports can be megabytes, that work was the whole of a save.
fn publish_dirty(sender: &Sender<Message>, state: &mut State) -> Res<()> {
    let mut roots: Vec<(String, PathBuf)> = state
        .ws
        .take_needs_publish()
        .into_iter()
        .filter_map(|root| {
            let path = state.ws.modules.get(root)?.path.clone();
            let key = state
                .docs
                .iter()
                .find(|(_, doc)| doc.path == path)
                .map(|(key, _)| key.clone())?;
            Some((key, path))
        })
        .collect();
    // Document order, so a client sees the edits and the publishes in the same
    // order however the set was built.
    roots.sort();
    for (key, path) in roots {
        if !state.ws.set_root_path(&path) {
            continue;
        }
        let uri: Uri = key
            .parse()
            .map_err(|e| format!("invalid document URI: {e}"))?;
        let version = state.docs.get(&key).and_then(|d| d.version);
        publish(sender, uri, &state.ws, version)?;
    }
    Ok(())
}

fn publish(sender: &Sender<Message>, uri: Uri, ws: &Workspace, version: Option<i32>) -> Res<()> {
    let diagnostics: Vec<Diagnostic> = analysis::diagnostics(ws)
        .into_iter()
        .map(|(range, message, severity)| {
            let code = analysis::diagnostic_code(&message);
            Diagnostic {
                range,
                severity: Some(match severity {
                    cagara_hir::workspace::Severity::Error => DiagnosticSeverity::ERROR,
                    cagara_hir::workspace::Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                code: Some(lsp_types::NumberOrString::String(code.into())),
                source: Some("cagara".into()),
                message,
                ..Default::default()
            }
        })
        .collect();
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version,
    };
    sender.send(Message::Notification(Notification::new(
        PublishDiagnostics::METHOD.into(),
        params,
    )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{serve, LogMessage, LogMessageParams, MessageType, PublishDiagnostics};
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
    // `LogMessage::METHOD` is a method of this trait, and `lsp_server` has a
    // `Notification` of its own, hence the anonymous import.
    use lsp_types::notification::Notification as _;
    use lsp_types::PublishDiagnosticsParams;
    use serde_json::json;
    use std::time::Duration;

    fn request(id: i32, method: &str, params: serde_json::Value) -> Message {
        Message::Request(Request {
            id: RequestId::from(id),
            method: method.into(),
            params,
        })
    }

    fn notify(method: &str, params: serde_json::Value) -> Message {
        Message::Notification(Notification {
            method: method.into(),
            params,
        })
    }

    /// The response to request `id`, skipping notifications.
    fn response(client: &Connection, id: i32) -> Response {
        loop {
            match client.receiver.recv_timeout(Duration::from_secs(30)) {
                Ok(Message::Response(r)) if r.id == RequestId::from(id) => return r,
                Ok(_) => {}
                Err(e) => panic!("no response to {id}: {e}"),
            }
        }
    }

    /// A window has to be able to say which compiler answers it: a binary that
    /// was staged or copied cannot be told from a current one by its path, and
    /// a copy carries the copy's timestamp. So the first thing the server says
    /// after initialization is the build it was made from.
    #[test]
    fn the_server_names_its_build() {
        let build =
            "0.3.1 (abc1234, release, x86_64-unknown-linux-gnu, built 2026-10-26 03:00 UTC)";
        let (server, client) = Connection::memory();
        let t = std::thread::spawn(move || serve(&server, build).map_err(|e| e.to_string()));
        client
            .sender
            .send(request(1, "initialize", json!({ "capabilities": {} })))
            .unwrap();
        response(&client, 1);
        client
            .sender
            .send(notify("initialized", json!({})))
            .unwrap();

        let message = client
            .receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("a message after initialized");
        let Message::Notification(n) = message else {
            panic!("expected the build announcement, got {message:?}")
        };
        assert_eq!(n.method, LogMessage::METHOD);
        let params: LogMessageParams = serde_json::from_value(n.params).expect("log message");
        assert_eq!(params.typ, MessageType::INFO);
        assert!(params.message.contains(build), "{}", params.message);

        client
            .sender
            .send(request(4, "shutdown", serde_json::Value::Null))
            .unwrap();
        response(&client, 4);
        client
            .sender
            .send(notify("exit", serde_json::Value::Null))
            .unwrap();
        t.join().unwrap().unwrap();
    }

    #[test]
    fn malformed_messages_do_not_end_the_session() {
        let (server, client) = Connection::memory();
        let t =
            std::thread::spawn(move || serve(&server, "0.3.1 (test)").map_err(|e| e.to_string()));
        client
            .sender
            .send(request(1, "initialize", json!({ "capabilities": {} })))
            .unwrap();
        response(&client, 1);
        client
            .sender
            .send(notify("initialized", json!({})))
            .unwrap();

        // Used to end the server: `?` on the params error left `run`.
        client
            .sender
            .send(request(2, "textDocument/hover", json!({ "bogus": 1 })))
            .unwrap();
        let r = response(&client, 2);
        let code = r.response_result.err().map(|e| e.code);
        assert_eq!(code, Some(ErrorCode::InvalidParams as i32));
        client
            .sender
            .send(notify("textDocument/didOpen", json!({ "bogus": 1 })))
            .unwrap();

        // Still serving.
        let uri = "file:///nonexistent/cagara-test/a.cagara";
        client
            .sender
            .send(notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": "cagara", "version": 1,
                    "text": "x = 1 + 2\n"
                } }),
            ))
            .unwrap();
        client
            .sender
            .send(request(
                3,
                "textDocument/hover",
                json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 0 } }),
            ))
            .unwrap();
        let r = response(&client, 3);
        let hover = r.response_result.expect("hover succeeds");
        assert!(!hover.is_null(), "no hover");

        client
            .sender
            .send(request(4, "shutdown", serde_json::Value::Null))
            .unwrap();
        response(&client, 4);
        client
            .sender
            .send(notify("exit", serde_json::Value::Null))
            .unwrap();
        t.join().unwrap().unwrap();
    }

    /// The documents a burst republishes, by file name.
    fn published(client: &Connection, quiet: Duration) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(message) = client.receiver.recv_timeout(quiet) {
            let Message::Notification(n) = message else {
                continue;
            };
            if n.method != PublishDiagnostics::METHOD {
                continue;
            }
            let params: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
            out.push(
                params
                    .uri
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        out.sort();
        out
    }

    /// Collect every diagnostic message the client was sent.
    fn messages(client: &Connection, quiet: Duration) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(message) = client.receiver.recv_timeout(quiet) {
            let Message::Notification(n) = message else {
                continue;
            };
            if n.method != PublishDiagnostics::METHOD {
                continue;
            }
            let params: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
            assert!(
                params.version.is_some(),
                "the document version is echoed back, so a stale publish is recognisable"
            );
            for d in params.diagnostics {
                assert!(d.code.is_some(), "every diagnostic carries a code: {d:?}");
                out.push(d.message);
            }
        }
        out
    }

    /// A ranged `didChange` is an *edit*, not a whole-document replacement.
    ///
    /// The session advertises incremental sync, so this is the path every
    /// keystroke takes. A server that ignored the range would keep only the
    /// replacement text — a file of one word — and the next publish would say
    /// so; a server that applied it to the wrong offset would report a
    /// different error. Both are visible here.
    #[test]
    fn a_ranged_change_edits_the_document() {
        let dir = std::env::temp_dir().join("cagara-lsp-incremental");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("q.cagara");
        let src = "t : query { a = int } = table \"s\" \"t\"\nq = t & where (.a > 1)\n";
        std::fs::write(&f, src).unwrap();

        let (server, client) = Connection::memory();
        let t =
            std::thread::spawn(move || serve(&server, "0.3.1 (test)").map_err(|e| e.to_string()));
        client
            .sender
            .send(request(1, "initialize", json!({ "capabilities": {} })))
            .unwrap();
        let _ = response(&client, 1);
        client
            .sender
            .send(notify("initialized", json!({})))
            .unwrap();
        let _ = client.receiver.recv_timeout(Duration::from_secs(30));
        let uri = format!("file://{}", f.display());
        client
            .sender
            .send(notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": "cagara", "version": 1, "text": src
                } }),
            ))
            .unwrap();
        let quiet = Duration::from_millis(600);
        assert!(messages(&client, quiet).is_empty(), "the file checks");

        // Replace `(.a > 1)` with `(.nope > 1)`: line 1, columns 13..21.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{
                        "range": {
                            "start": { "line": 1, "character": 13 },
                            "end": { "line": 1, "character": 21 }
                        },
                        "text": "(.nope > 1)"
                    }]
                }),
            ))
            .unwrap();
        let found = messages(&client, quiet);
        assert!(
            found.iter().any(|m| m.contains("no column `nope`")),
            "the edited text was not checked: {found:?}"
        );
        // A whole-document change still works, which is what a client that
        // ignores the capability sends.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": 3 },
                    "contentChanges": [{ "text": src }]
                }),
            ))
            .unwrap();
        assert!(
            messages(&client, quiet).is_empty(),
            "a full replacement with the original text checks again"
        );

        client
            .sender
            .send(request(2, "shutdown", serde_json::Value::Null))
            .unwrap();
        let _ = response(&client, 2);
        let _ = t.join().unwrap();
    }

    /// A save republishes the documents whose modules changed, and no others.
    ///
    /// Every save used to publish every open document, three times over: once
    /// from `didSave`, once from the watched-file event that the same write
    /// raises, and once from the refresh behind a burst. On a project whose
    /// library files run to megabytes, that was the whole cost of a save — N
    /// compilations to discover that N-1 documents still say the same thing.
    #[test]
    fn a_save_publishes_only_the_documents_that_changed() {
        let dir = std::env::temp_dir().join("cagara-lsp-publish");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.cagara");
        let b = dir.join("b.cagara");
        let lib = dir.join("lib.cagara");
        let a_src = "import \"lib.cagara\"\nqa = value\n";
        let b_src = "qb = 1 + 1\n";
        let lib_src = "value = 1\n";
        std::fs::write(&a, a_src).unwrap();
        std::fs::write(&b, b_src).unwrap();
        std::fs::write(&lib, lib_src).unwrap();

        let (server, client) = Connection::memory();
        let t =
            std::thread::spawn(move || serve(&server, "0.3.1 (test)").map_err(|e| e.to_string()));
        client
            .sender
            .send(request(1, "initialize", json!({ "capabilities": {} })))
            .unwrap();
        let _ = response(&client, 1);
        client
            .sender
            .send(notify("initialized", json!({})))
            .unwrap();
        // The build announcement.
        let _ = client.receiver.recv_timeout(Duration::from_secs(30));

        let uri = |p: &std::path::Path| format!("file://{}", p.display());
        for (path, text) in [(&a, a_src), (&b, b_src), (&lib, lib_src)] {
            client
                .sender
                .send(notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri(path), "languageId": "cagara", "version": 1, "text": text
                    } }),
                ))
                .unwrap();
        }
        // Opening publishes each document as it arrives.
        assert_eq!(
            published(&client, Duration::from_millis(600)),
            vec!["a.cagara", "b.cagara", "lib.cagara"]
        );

        let quiet = Duration::from_millis(600);

        // Typing in `a` publishes `a`.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri(&a), "version": 2 },
                    "contentChanges": [{ "text": "import \"lib.cagara\"\nqa = value + 1\n" }]
                }),
            ))
            .unwrap();
        assert_eq!(published(&client, quiet), vec!["a.cagara"]);

        // Saving it publishes nothing: no module moved.
        client
            .sender
            .send(notify(
                "textDocument/didSave",
                json!({
                    "textDocument": { "uri": uri(&a) },
                    "text": "import \"lib.cagara\"\nqa = value + 1\n"
                }),
            ))
            .unwrap();
        assert!(
            published(&client, quiet).is_empty(),
            "a save of unchanged modules republished"
        );

        // Editing the library republishes it and the document that reads it,
        // and not the unrelated one.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri(&lib), "version": 2 },
                    "contentChanges": [{ "text": "value = 2\n" }]
                }),
            ))
            .unwrap();
        assert_eq!(published(&client, quiet), vec!["a.cagara", "lib.cagara"]);

        client
            .sender
            .send(request(2, "shutdown", serde_json::Value::Null))
            .unwrap();
        let _ = response(&client, 2);
        client
            .sender
            .send(notify("exit", serde_json::Value::Null))
            .unwrap();
        t.join().unwrap().unwrap();
    }

    /// A diagnostic a fix exists for comes with that fix, and a fix built from
    /// the wrong span would rewrite the wrong text — so the edit is asserted,
    /// not just its title.
    ///
    /// Two rules are mechanical: a bare sort key (`asc (…)` / `desc (…)`), and a
    /// name a "did you mean" already resolved. The join warning and the nullable
    /// coercion are deliberately absent from `analysis::code_actions`: their
    /// messages name the problem but not the span to rewrite.
    #[test]
    fn a_fixable_diagnostic_comes_with_its_fix() {
        let dir = std::env::temp_dir().join("cagara-lsp-fixes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("q.cagara");
        let src = "t : query { a = int } = table \"s\" \"t\"\n\
                   q = t & where (.a > 0)\n\
                   unsorted = t & order [.a]\n\
                   bad : intt = 1\n";
        std::fs::write(&f, src).unwrap();

        let (server, client) = Connection::memory();
        let t =
            std::thread::spawn(move || serve(&server, "0.4.1 (test)").map_err(|e| e.to_string()));
        client
            .sender
            .send(request(1, "initialize", json!({ "capabilities": {} })))
            .unwrap();
        let _ = response(&client, 1);
        client
            .sender
            .send(notify("initialized", json!({})))
            .unwrap();
        let _ = client.receiver.recv_timeout(Duration::from_secs(30));
        let uri = format!("file://{}", f.display());
        client
            .sender
            .send(notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": "cagara", "version": 1, "text": src
                } }),
            ))
            .unwrap();
        let quiet = Duration::from_millis(600);
        assert!(
            !messages(&client, quiet).is_empty(),
            "the file has an error and a warning"
        );

        // The whole document, so every diagnostic with a fix answers.
        let whole = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 99, "character": 0 }
            },
            "context": { "diagnostics": [] }
        });
        client
            .sender
            .send(request(2, "textDocument/codeAction", whole))
            .unwrap();
        let r = response(&client, 2);
        let actions: Vec<serde_json::Value> =
            serde_json::from_value(r.response_result.unwrap()).unwrap();
        let titles: Vec<&str> = actions
            .iter()
            .filter_map(|a| a.get("title").and_then(|t| t.as_str()))
            .collect();
        assert!(
            titles.iter().any(|t| t.contains("asc")),
            "no `asc` fix: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.contains("desc")),
            "no `desc` fix: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.contains("Rename to `int`")),
            "no rename fix: {titles:?}"
        );
        // `changes` is a map of URI to edits, so the edit is found by value.
        let texts: Vec<&str> = actions
            .iter()
            .filter_map(|a| a.pointer("/edit/changes").and_then(|c| c.as_object()))
            .filter_map(|c| c.values().next())
            .filter_map(|edits| edits.get(0))
            .filter_map(|e| e.get("newText").and_then(|t| t.as_str()))
            .collect();
        assert!(texts.contains(&"asc (.a)"), "edits: {texts:?}");
        assert!(texts.contains(&"desc (.a)"), "edits: {texts:?}");
        assert!(texts.contains(&"int"), "edits: {texts:?}");

        // Inlay hints: the inferred type of a definition that does not write
        // one, and none for the one that does.
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 99, "character": 0 }
            }
        });
        client
            .sender
            .send(request(3, "textDocument/inlayHint", params))
            .unwrap();
        let r = response(&client, 3);
        let hints: Vec<serde_json::Value> =
            serde_json::from_value(r.response_result.unwrap()).unwrap();
        let labels: Vec<&str> = hints
            .iter()
            .filter_map(|h| h.get("label").and_then(|l| l.as_str()))
            .collect();
        assert!(
            labels.iter().any(|l| l.contains("query { a = int }")),
            "no type hint for `q`: {labels:?}"
        );
        // `t` and `bad` write their types; `unsorted` failed to check, so it has
        // none to show. One hint, therefore.
        assert_eq!(labels.len(), 1, "hints: {labels:?}");

        // The join warning's fix comes from the diagnostic itself: wrap the
        // right operand and rename the predicate's reference to the column.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{
                        "text": "a : query { id = int, x = int } = table \"s\" \"a\"\n\
                                 b : query { id = int, y = int } = table \"s\" \"b\"\n\
                                 q = a & innerJoin b (.<id == .>id)\n"
                    }]
                }),
            ))
            .unwrap();
        let _ = messages(&client, quiet);
        let whole = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 99, "character": 0 }
            },
            "context": { "diagnostics": [] }
        });
        client
            .sender
            .send(request(5, "textDocument/codeAction", whole))
            .unwrap();
        let r = response(&client, 5);
        let actions: Vec<serde_json::Value> =
            serde_json::from_value(r.response_result.unwrap()).unwrap();
        let join = actions
            .iter()
            .find(|a| {
                a.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.contains("right_"))
            })
            .unwrap_or_else(|| panic!("no join rename fix: {actions:?}"));
        let edits = join
            .pointer("/edit/changes")
            .and_then(|c| c.as_object())
            .and_then(|c| c.values().next())
            .and_then(|e| e.as_array())
            .unwrap_or_else(|| panic!("no edits: {join:?}"));
        let texts: Vec<&str> = edits
            .iter()
            .filter_map(|e| e.get("newText").and_then(|t| t.as_str()))
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("& select { right_id = .id })")),
            "the operand is not renamed: {texts:?}"
        );
        assert!(
            texts.contains(&".>right_id"),
            "the predicate is not retokened: {texts:?}"
        );

        client
            .sender
            .send(request(4, "shutdown", serde_json::Value::Null))
            .unwrap();
        let _ = response(&client, 4);
        let _ = t.join().unwrap();
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::apply_change;
    use lsp_types::{Position, Range};

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        }
    }

    /// A range edit replaces exactly the text between the two UTF-16
    /// positions, and nothing else.
    #[test]
    fn a_range_edit_replaces_what_it_covers() {
        let text = "q = users & where (.age > 18)\n";
        // `users` -> `people`.
        let out = apply_change(text, range(0, 4, 0, 9), "people");
        assert_eq!(out, "q = people & where (.age > 18)\n");
        // An insertion (empty range) at the end of a line: after `)`.
        let out = apply_change(text, range(0, 29, 0, 29), " & limit 1");
        assert_eq!(out, "q = users & where (.age > 18) & limit 1\n");
        // A deletion.
        let out = apply_change(text, range(0, 3, 0, 9), "");
        assert_eq!(out, "q = & where (.age > 18)\n");
        // Across lines: everything from the end of the first line to the end
        // of the second is one range, and replacing it joins the text.
        let text = "a = 1\nb = 2\n";
        let out = apply_change(text, range(0, 5, 1, 5), " & 1");
        assert_eq!(out, "a = 1 & 1\n");
    }

    /// Columns are UTF-16 code units, not bytes: an emoji before the edit point
    /// is two columns wide, and the edit still lands where the client meant.
    #[test]
    fn columns_count_utf16_units() {
        let text = "s = \"🎉\" & upper \"x\"\n";
        // `upper` starts at column 11 (the emoji counts as two).
        let out = apply_change(text, range(0, 11, 0, 16), "lower");
        assert_eq!(out, "s = \"🎉\" & lower \"x\"\n");
    }

    /// A position that does not fit the text leaves it alone rather than
    /// panicking or slicing inside a character: a client and server that
    /// disagree about the version must not end the session.
    #[test]
    fn a_stale_range_changes_nothing() {
        let text = "q = 1\n";
        assert_eq!(apply_change(text, range(9, 0, 9, 3), "x"), text);
        assert_eq!(apply_change(text, range(0, 10, 0, 12), "x"), text);
        // Inside the emoji: column 5 is the low half of the surrogate pair.
        let text = "s = \"🎉\"\n";
        assert_eq!(apply_change(text, range(0, 6, 0, 7), "x"), text);
    }
}
