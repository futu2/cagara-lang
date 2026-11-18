//! The language server over stdio (`cagara lsp`). Incremental sync (a change is
//! a range to apply, see `apply_change`); each open document is the root of its
//! own workspace, updated with `set_source` (reloaded when its imports change).
//! Imported files are read from disk.

use crate::{analysis, uri};
use cagara_hir::workspace::Workspace;
use cagara_sql::Dialect;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    Cancel, DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles,
    DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument, LogMessage, Notification as _,
    PublishDiagnostics, ShowMessage,
};
use lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest,
    FoldingRangeRequest, Formatting, GotoDefinition, HoverRequest, InlayHintRequest,
    PrepareRenameRequest, References, Rename, Request as _, SemanticTokensFullRequest,
    SignatureHelpRequest, WorkspaceSymbolRequest,
};
use lsp_types::{
    CancelParams, CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CompletionOptions, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse, FoldingRange,
    FoldingRangeParams, FoldingRangeProviderCapability, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, LogMessageParams, MarkupContent,
    MarkupKind, MessageType, NumberOrString, OneOf, PrepareRenameResponse,
    PublishDiagnosticsParams, ReferenceParams, RenameOptions, RenameParams, SaveOptions,
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ShowMessageParams, SignatureHelp,
    SignatureHelpOptions, SignatureInformation, SymbolInformation, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, WorkspaceEdit, WorkspaceSymbolParams,
};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
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
    /// The target the documents are written for, from `cagara.dialect`.
    ///
    /// [`Dialect::Ansi`] until the client says otherwise, which is the right fallback:
    /// ANSI is the permissive target, so a document whose target nobody stated gets
    /// only the refusals that are about the program. Once it is set, the diagnostics
    /// here are the ones `cagara --dialect <that>` reports — see
    /// `analysis::diagnostics_with`.
    dialect: Dialect,
}

impl State {
    fn new() -> Self {
        Self {
            docs: HashMap::new(),
            ws: Workspace::from_source(""),
            dialect: Dialect::Ansi,
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
/// arrive and pile up here instead of waiting to be read. [`next_batch`] then
/// merges a burst of changes for one document into one notification, so every
/// edit is applied in order and the file is checked once — of the text the
/// user is actually looking at, not of a keystroke in the middle of the burst.
///
/// Answers are sent by the analysis thread over the same sender as the
/// diagnostics, so this loop never waits for one. That is what lets a
/// `$/cancelRequest` be read and honoured while the request it names is still
/// being computed: when the router waited on the answer, the cancel sat in the
/// socket until the answer nobody wanted had already gone out.
fn serve(conn: &Connection, build: &str) -> Res<()> {
    let caps = ServerCapabilities {
        // The options form, not the bare `TextDocumentSyncKind::INCREMENTAL`.
        // The bare form is a number, and a number has no room to say anything
        // about saving — so a client reads it as "no save notifications", and
        // the `didSave` handler below could never run. Measured: VS Code
        // registers its `DidSaveTextDocumentFeature` only when
        // `textDocumentSyncOptions.save` is set, so with the bare form the
        // handler was dead code that also made the save path untested.
        //
        // `change` stays incremental: the client sends the range it changed,
        // not the whole document. A 2000-line file costs a few dozen bytes per
        // keystroke instead of the file, and the server applies the edit to the
        // text it already has — see `apply_change`.
        //
        // `include_text` is asked for because it is what makes the save
        // authoritative. Without it a client may send only the URI, and a save
        // then has to be *inferred* from the file on disk; with it the text the
        // client saved arrives with the notification, which is the one thing a
        // client cannot be wrong about. A client that ignores it sends no text,
        // and the handler falls back to the file.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(true),
                })),
                ..Default::default()
            },
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        // Which argument of the call the cursor is in. `hover` says what a name is;
        // this says what the *next* argument should be, which is the question a
        // signature popup exists to answer.
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: None,
            retrigger_characters: None,
            work_done_progress_options: Default::default(),
        }),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        // `analysis::rename` rewrites a definition's name and every use of it, in
        // every file that imports it, and a lambda parameter's name within its scope.
        // `prepare_rename_provider` is advertised with it: an editor that offers the
        // rename gesture needs to know, before the user types, whether the cursor is
        // on something renameable — a column name is not.
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        document_highlight_provider: Some(OneOf::Left(true)),
        // A definition in *any* open file. `documentSymbol` answers for one document,
        // which is what an outline needs; this is what "go to symbol" needs.
        workspace_symbol_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        // Definitions whose body spans lines, `---` doc blocks, and `catalog` blocks.
        // A file of table declarations is otherwise a wall of text an editor cannot
        // collapse.
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        // Only what a grammar cannot know: whether a name is a table, a column, a
        // parameter or a function is resolution, not spelling.
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: analysis::TOKEN_TYPES
                        .iter()
                        .map(|t| SemanticTokenType::new(t))
                        .collect(),
                    token_modifiers: Vec::new(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(false),
                ..Default::default()
            },
        )),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        // The inferred type of a definition that does not write one, at the end
        // of its name (`analysis::type_hints`).
        inlay_hint_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        // The dialect arrives through `initializationOptions` and changes through
        // `workspace/didChangeConfiguration`. There is no server capability that
        // advertises the latter — the protocol has the client send it to every server
        // it has a configuration section for — so nothing is declared for it here. The
        // handler below is what makes it work, and the VS Code client is configured to
        // send it (see `editors/vscode`).
        ..Default::default()
    };
    // The return value is the client's `initialize` params, which is where the
    // configured dialect arrives. `Connection::initialize` builds the reply itself,
    // so this is the one place the request is visible without re-implementing it.
    let init_params = conn.initialize(serde_json::to_value(caps)?)?;
    let dialect = dialect_from_initialize(&init_params);
    if dialect != Dialect::Ansi {
        conn.sender.send(Message::Notification(Notification::new(
            LogMessage::METHOD.to_string(),
            serde_json::to_value(LogMessageParams {
                typ: MessageType::INFO,
                message: format!("cagara: dialect `{}`", cagara_sql::dialect::name(dialect)),
            })?,
        )))?;
    }
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
    // The client's cancels, read by the router and honoured by the analysis
    // thread (see `Cancellations`).
    let cancelled = Arc::new(Cancellations::default());
    let sender = conn.sender.clone();
    let worker = std::thread::Builder::new()
        .name("cagara-analysis".into())
        .spawn({
            let cancelled = Arc::clone(&cancelled);
            move || analyse(jobs, sender, cancelled, dialect)
        })?;
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
                // Queued and not awaited: the analysis thread sends the answer
                // itself, so this loop goes straight back to reading. That is
                // the whole point — while a slow request is being computed, the
                // next keystroke, the next request and the cancel for this one
                // are still read instead of sitting in the socket.
                cancelled.queued(&id);
                if work.send(Work::Request { id, method, params }).is_err() {
                    break;
                }
            }
            Message::Notification(n) => {
                // Handled here, not forwarded: the analysis thread has no
                // handler for it, and a cancel queued behind the request it
                // names would be read only after that request was answered —
                // which is exactly the request the client wanted stopped.
                if n.method == Cancel::METHOD {
                    match serde_json::from_value::<CancelParams>(n.params) {
                        Ok(p) => cancelled.cancel(request_id(p.id)),
                        // A cancel without an id names nothing. It is not a
                        // reason to end the session.
                        Err(e) => {
                            eprintln!("cagara lsp: {}: {e}", Cancel::METHOD);
                        }
                    }
                    continue;
                }
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
    // The analysis thread ends when its channel closes. It is joined *before*
    // anything is answered from here: once it is gone, an id still in flight is
    // provably unanswered, so this cannot turn into a second response to a
    // request it did answer. A request whose answer never came is still a
    // request the client is waiting on, so it gets an error rather than
    // silence.
    drop(work);
    let _ = worker.join();
    for id in cancelled.unfinished() {
        let _ = conn.sender.send(Message::Response(Response::new_err(
            id,
            ErrorCode::InternalError as i32,
            "internal error: analysis stopped before answering".into(),
        )));
    }
    Ok(())
}

/// A protocol id as a `RequestId`: the id on the wire is a number or a string,
/// which is the pair `RequestId` holds.
fn request_id(id: NumberOrString) -> RequestId {
    match id {
        NumberOrString::Number(n) => RequestId::from(n),
        NumberOrString::String(s) => RequestId::from(s),
    }
}

/// How many cancels the router remembers before the request they name arrives.
///
/// Messages are read in order, so a cancel is normally for a request that is
/// already queued — the in-flight set answers that without any remembering.
/// This only has to cover the odd cancel read before its request; an id
/// evicted from here is simply computed, which is what the server did before
/// it honoured cancels at all.
const REMEMBERED_CANCELS: usize = 64;

/// The requests a client has cancelled, and the ones still waiting for an
/// answer.
///
/// Shared by the router and the analysis thread because they never otherwise
/// speak about a request once it is queued: the answer goes straight to the
/// client. The router is the only writer of cancels — a `$/cancelRequest` is
/// read there and never reaches `notification()` — and the analysis thread is
/// the reader, both before it computes (so a cancelled request costs nothing)
/// and again before it replies (a cancel that arrived while the answer was
/// being computed must not be answered with a result the client no longer
/// wants).
#[derive(Default)]
struct Cancellations {
    /// `VecDeque` rather than a set: the cap is a count of recent ids, and the
    /// oldest is the one worth forgetting.
    cancelled: Mutex<std::collections::VecDeque<RequestId>>,
    /// Queued and not yet answered, so a cancel for a request that will arrive
    /// can be told from one for a request that never will, and so a shutdown
    /// can answer every request the client is still waiting on.
    in_flight: Mutex<HashSet<RequestId>>,
}

impl Cancellations {
    /// Both sets only ever have an id inserted or removed, so a lock poisoned
    /// by a panic elsewhere is still consistent; taking it rather than
    /// unwrapping is what keeps one panic from making the server unable to
    /// answer anything afterwards.
    fn cancelled(&self) -> MutexGuard<'_, std::collections::VecDeque<RequestId>> {
        self.cancelled.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn in_flight(&self) -> MutexGuard<'_, HashSet<RequestId>> {
        self.in_flight.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The router: a request is on its way to the analysis thread.
    fn queued(&self, id: &RequestId) {
        self.in_flight().insert(id.clone());
    }

    /// The router: the client asked for `id` to be cancelled.
    fn cancel(&self, id: RequestId) {
        let mut cancelled = self.cancelled();
        cancelled.retain(|known| known != &id);
        cancelled.push_back(id);
        if cancelled.len() > REMEMBERED_CANCELS {
            cancelled.pop_front();
        }
    }

    /// The analysis thread: has this request been cancelled?
    fn is_cancelled(&self, id: &RequestId) -> bool {
        self.cancelled().contains(id)
    }

    /// The analysis thread: this request has its answer, so forget both marks.
    fn answered(&self, id: &RequestId) {
        self.cancelled().retain(|known| known != id);
        self.in_flight().remove(id);
    }

    /// The router, once the analysis thread has ended: the ids it never
    /// answered.
    fn unfinished(&self) -> Vec<RequestId> {
        self.in_flight().iter().cloned().collect()
    }
}

/// The answer to a request the client cancelled.
///
/// The protocol requires a response to every request, so a cancelled one is
/// answered rather than dropped — with the error code that says "you asked me
/// to stop", so the editor can discard it instead of drawing a stale hover.
fn cancelled_fail(method: &str) -> Fail {
    (
        ErrorCode::RequestCanceled,
        format!("{method}: cancelled by the client"),
    )
}

/// Work for the analysis thread.
enum Work {
    /// A request to answer. It carries its id rather than a reply channel: the
    /// answer is a `Message::Response` sent to the client directly, so the
    /// router never has to wait for one to keep reading.
    Request {
        id: RequestId,
        method: String,
        params: serde_json::Value,
    },
    Notify(Notification),
    /// Refresh the diagnostics of every open document.
    Publish,
}

/// Everything already queued, with each document's burst of changes merged
/// into one notification.
struct Batch {
    notify: Vec<Notification>,
    requests: Vec<Work>,
    publish: bool,
}

/// The analysis thread: own the workspace, do the work, never read stdin.
///
/// Notifications are handled before the requests of the same batch, and that
/// order is deliberate: a request is read from a client that has already
/// applied its own edits, so the text it asks about is the newest one. Running
/// the batch's notifications first means a request that was queued behind a
/// keystroke answers about the text that keystroke produced — never about the
/// text the client has already moved past. (A request that has to see older
/// text is a request for a versioned document, which this server does not
/// keep.)
fn analyse(
    jobs: Receiver<Work>,
    sender: Sender<Message>,
    cancelled: Arc<Cancellations>,
    dialect: Dialect,
) {
    let mut state = State::new();
    state.dialect = dialect;
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
            let Work::Request { id, method, params } = job else {
                unreachable!("only requests are collected here");
            };
            // Checked before the work and again after it: a cancel can arrive
            // while the answer is being computed, and answering with the result
            // it cancelled is the one thing a cancel exists to prevent. A
            // cancelled request is still answered — with `RequestCancelled` —
            // because a client is waiting for a response either way.
            let answer = if cancelled.is_cancelled(&id) {
                Err(cancelled_fail(&method))
            } else {
                match catch_unwind(AssertUnwindSafe(|| request(&mut state, &method, params))) {
                    Ok(result) => result,
                    Err(p) => {
                        rebuild(&mut state);
                        Err((
                            ErrorCode::InternalError,
                            format!("internal error in {method}: {}", panic_message(&*p)),
                        ))
                    }
                }
            };
            let answer = if cancelled.is_cancelled(&id) {
                Err(cancelled_fail(&method))
            } else {
                answer
            };
            cancelled.answered(&id);
            let resp = match answer {
                Ok(v) => Response::new_ok(id, v),
                Err((code, msg)) => Response::new_err(id, code as i32, msg),
            };
            // The writer thread is gone when this fails; there is nobody left
            // to answer, so stop rather than keep analysing.
            if sender.send(Message::Response(resp)).is_err() {
                return;
            }
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
/// Requests are all kept — each needs its own answer — and so is every change:
/// a burst of edits for one document is merged into a single `didChange`
/// carrying all of them (see [`coalesce_changes`]), which is what makes the
/// analysis run once per document per burst without losing an edit.
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
    batch.notify = coalesce_changes(batch.notify);
    Some(batch)
}

/// Merge a run of `didChange` notifications for one document into one.
///
/// A ranged edit is relative to the text the edit before it produced, so every
/// change in a burst has to be applied, in order. Keeping only the last one —
/// which is what this used to do — applied its range to text it was never
/// written against: a burst that renamed a definition and then edited another
/// line lost the rename, because the rename's text existed only in the change
/// that was dropped. What can be coalesced is the analysis, not the edits: the
/// merged notification carries all of a burst's changes and the newest version,
/// and the analysis thread's `for change in p.content_changes` applies them in
/// order, so a burst of ten keystrokes still costs one check of the text the
/// user is looking at.
///
/// The merged notification sits where the first one was, so a document's
/// changes are still handled in the order the client sent them relative to
/// everything else. A `didOpen`, `didSave` or `didClose` for the same document
/// is a barrier: those reset or overwrite the text the earlier changes were
/// relative to, so changes on either side of one are never merged. A
/// notification for another document is not a barrier — it does not touch this
/// document's text.
fn coalesce_changes(notifications: Vec<Notification>) -> Vec<Notification> {
    // Where each document's open run of changes sits in `out`. A document has
    // an entry exactly while changes after its last barrier are being merged.
    let mut open: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Notification> = Vec::with_capacity(notifications.len());
    for n in notifications {
        let Some(uri) = text_document_uri(&n) else {
            out.push(n);
            continue;
        };
        match n.method.as_str() {
            DidChangeTextDocument::METHOD => match open.get(&uri) {
                Some(&at) => merge_changes(&mut out[at], n),
                None => {
                    open.insert(uri, out.len());
                    out.push(n);
                }
            },
            // The only other notifications with a `textDocument` are the
            // barriers: they close the run, so the next change for this
            // document starts a new notification after them.
            _ => {
                open.remove(&uri);
                out.push(n);
            }
        }
    }
    out
}

/// The document a notification opens, edits, saves or closes, by URI.
fn text_document_uri(n: &Notification) -> Option<String> {
    match n.method.as_str() {
        DidOpenTextDocument::METHOD
        | DidChangeTextDocument::METHOD
        | DidSaveTextDocument::METHOD
        | DidCloseTextDocument::METHOD => n
            .params
            .pointer("/textDocument/uri")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// Move `later`'s changes onto the end of `earlier`, and take its version.
///
/// The version has to be the client's newest: the merged notification produces
/// the text of the last change, and diagnostics echo the version back, so a
/// stale number would let a client read a current publish as out of order.
fn merge_changes(earlier: &mut Notification, mut later: Notification) {
    // Read the version before taking the changes: `take` leaves a null behind,
    // but the two live in different objects and a reader should not have to
    // check that.
    let version = later
        .params
        .pointer("/textDocument/version")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let more = later
        .params
        .get_mut("contentChanges")
        .map(serde_json::Value::take);
    if let (Some(serde_json::Value::Array(changes)), Some(serde_json::Value::Array(more))) =
        (earlier.params.get_mut("contentChanges"), more)
    {
        changes.extend(more);
    }
    earlier.params["textDocument"]["version"] = version;
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
            // Each reference carries its own path: a use can be in a file that
            // imports the definition, and a `Location` naming the requesting document
            // would open the wrong file at a plausible position.
            let locs: Option<Vec<Location>> = state.select(&key).then(|| {
                analysis::references(&state.ws, tp.position, p.context.include_declaration)
                    .into_iter()
                    .filter_map(|(path, range)| {
                        Some(Location {
                            uri: uri::from_path(&path)?,
                            range,
                        })
                    })
                    .collect()
            });
            json(serde_json::to_value(locs))
        }
        PrepareRenameRequest::METHOD => {
            let p: TextDocumentPositionParams =
                serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let r: Option<PrepareRenameResponse> = state
                .select(&key)
                .then(|| {
                    analysis::prepare_rename(&state.ws, p.position)
                        .ok()
                        .map(PrepareRenameResponse::Range)
                })
                .flatten();
            json(serde_json::to_value(r))
        }
        Rename::METHOD => {
            let p: RenameParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document_position.text_document.uri);
            let outcome = if state.select(&key) {
                analysis::rename(&state.ws, p.text_document_position.position, &p.new_name)
            } else {
                // A document the server was never told about: nothing to rename, and
                // an empty edit set says that without pretending to have looked.
                analysis::Rename::Edits(Vec::new())
            };
            match outcome {
                analysis::Rename::Edits(files) => {
                    // An empty edit set is a valid answer: the file already says what
                    // was asked for. `null` would be read as "no such name", which is
                    // a different thing.
                    // `lsp_types::Uri` has interior mutability, which `clippy` reads as
                    // a mutable key. It is what the protocol's `changes` map is keyed
                    // by, so the allow sits on the map rather than on the method.
                    #[allow(clippy::mutable_key_type)]
                    let mut changes: HashMap<Uri, Vec<lsp_types::TextEdit>> = HashMap::new();
                    for (path, edits) in files {
                        if let Some(u) = uri::from_path(&path) {
                            changes.insert(u, edits);
                        }
                    }
                    json(serde_json::to_value(Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    })))
                }
                // A refusal is a request error, not an empty edit: an editor that got
                // `null` would say "nothing to change" and the user would believe the
                // rename happened.
                analysis::Rename::Refused(why) => Err((ErrorCode::InvalidRequest, why)),
            }
        }
        SemanticTokensFullRequest::METHOD => {
            let p: SemanticTokensParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let data: Option<SemanticTokensResult> = state.select(&key).then(|| {
                let tokens = analysis::semantic_tokens(&state.ws);
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: analysis::encode_tokens(&tokens)
                        .into_iter()
                        .map(
                            |[delta_line, delta_start, length, token_type, token_modifiers]| {
                                SemanticToken {
                                    delta_line,
                                    delta_start,
                                    length,
                                    token_type,
                                    token_modifiers_bitset: token_modifiers,
                                }
                            },
                        )
                        .collect(),
                })
            });
            json(serde_json::to_value(data))
        }
        WorkspaceSymbolRequest::METHOD => {
            let _p: WorkspaceSymbolParams = serde_json::from_value(params).map_err(bad_params)?;
            // Every open document shares one module graph (`State::ws`), so the search
            // runs once over it rather than once per document. The root is whichever
            // was selected last, and every open buffer is in the graph, so the answer
            // covers them all.
            let found = analysis::workspace_symbols(&state.ws);
            let mut syms: Vec<SymbolInformation> = found
                .into_iter()
                .filter_map(|(path, range, name, kind)| {
                    // A path the editor cannot open is not a location. The prelude is
                    // filtered out in `workspace_symbols`; this is the same rule for
                    // anything else without a real path.
                    let u = uri::from_path(&path)?;
                    Some(SymbolInformation {
                        name,
                        kind,
                        tags: None,
                        #[allow(deprecated)]
                        deprecated: None,
                        location: Location { uri: u, range },
                        container_name: None,
                    })
                })
                .collect();
            // Stable order, so the editor's list does not reshuffle between keystrokes.
            syms.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then_with(|| a.location.uri.as_str().cmp(b.location.uri.as_str()))
            });
            json(serde_json::to_value(Some(syms)))
        }
        SignatureHelpRequest::METHOD => {
            let p: TextDocumentPositionParams =
                serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let help: Option<SignatureHelp> = state
                .select(&key)
                .then(|| analysis::signature_help(&state.ws, p.position))
                .flatten()
                .map(|(sigs, active)| SignatureHelp {
                    signatures: sigs
                        .into_iter()
                        .map(|label| SignatureInformation {
                            label,
                            documentation: None,
                            parameters: None,
                            active_parameter: Some(active as u32),
                        })
                        .collect(),
                    active_signature: Some(0),
                    active_parameter: Some(active as u32),
                });
            json(serde_json::to_value(help))
        }
        FoldingRangeRequest::METHOD => {
            let p: FoldingRangeParams = serde_json::from_value(params).map_err(bad_params)?;
            let key = doc_key(&p.text_document.uri);
            let ranges: Option<Vec<FoldingRange>> = state
                .select(&key)
                .then(|| analysis::folding_ranges(&state.ws));
            json(serde_json::to_value(ranges))
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
            let key = doc_key(&p.text_document.uri);
            // A client that was asked for `includeText` sends the text it
            // saved, and that text is the one thing it cannot be wrong about:
            // it is what is now on disk. It is normally the text we already
            // have — a save writes the buffer — so this is a resynchronisation
            // rather than an edit, and the comparison is what keeps it cheap:
            // `set_source` re-parses and drops the compile caches even when the
            // text is identical, which would republish a document nothing
            // changed in.
            //
            // It used to assign `doc.text` and stop there, which left the
            // workspace — the thing the analysis actually reads — holding the
            // pre-save text, so the two copies of the document disagreed. With
            // the save notification unreachable (the capability advertised a
            // bare sync kind, which has no room to mention saving) that was
            // unobservable; advertising the save is what makes it a bug worth
            // fixing in the same change.
            if let (Some(doc), Some(text)) = (state.docs.get_mut(&key), p.text) {
                if text != doc.text {
                    doc.text = text;
                    let text = doc.text.clone();
                    let patched = state
                        .ws
                        .module_for_path(&doc.path)
                        .is_some_and(|m| state.ws.set_source(m, text));
                    if !patched {
                        state.rebuild();
                    }
                }
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
        // The target moved. Every open document's diagnostics depend on it now — a
        // `groups` frame is refused for MySQL and fine for DuckDB — so the whole set
        // is republished rather than only the roots the workspace considers dirty. It
        // is a settings change, not a keystroke: it happens once, and a document whose
        // squiggle did not move is cheap to leave alone only if nothing recomputed it,
        // which is the opposite of what changed here.
        DidChangeConfiguration::METHOD => {
            // `DidChangeConfigurationParams` is `{ settings: Value }`, and the server
            // does not ask for `workspace/configuration` because that is a *request*
            // from server to client, which this loop answers rather than makes. So the
            // settings are read from what the client sent.
            let _: lsp_types::DidChangeConfigurationParams =
                serde_json::from_value(n.params.clone())?;
            let settings = n.params.get("settings").cloned().unwrap_or_default();
            let Some((dialect, unknown)) = dialect_from_settings(&settings) else {
                return Ok(());
            };
            if let Some(name) = unknown {
                // Reported, not fatal: a misspelled dialect must not take the session
                // down, and the fallback keeps the diagnostics coming.
                sender.send(Message::Notification(Notification::new(
                    ShowMessage::METHOD.to_string(),
                    serde_json::to_value(ShowMessageParams {
                        typ: MessageType::WARNING,
                        message: format!(
                            "cagara: unknown dialect `{name}`; using `ansi`. Known: \
                             ansi, postgres, duckdb, sqlite, mysql, trino, spark, bigquery, \
                             snowflake, tsql, and the aliases each of those accepts"
                        ),
                    })?,
                )))?;
            }
            if dialect == state.dialect {
                return Ok(());
            }
            state.dialect = dialect;
            // Every open document, because the answer changed for all of them. The
            // set is the open tabs, not the tree, so this is a handful of documents —
            // and it is a settings change, so it happens once rather than per keystroke.
            let keys: Vec<String> = state.docs.keys().cloned().collect();
            for key in keys {
                publish_doc(sender, state, &key)?;
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
    publish(sender, uri, &state.ws, version, state.dialect)?;
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
        publish(sender, uri, &state.ws, version, state.dialect)?;
    }
    Ok(())
}

/// The dialect named in a client's `initialize` params, or ANSI.
///
/// Two spellings are accepted, because the two clients this project has differ: VS Code
/// passes the whole `cagara` namespace (`{ "cagara": { "dialect": "mysql" } }`, which is
/// what `contributes.configuration` produces), and anything else may send the flat form
/// (`{ "dialect": "mysql" }`). Reading both means a client does not have to know which
/// one the server prefers.
///
/// An unrecognized name is *not* an error here: it is reported once to the client and
/// the fallback is used, because a save-time crash over a config value is worse than a
/// diagnostic computed against ANSI.
fn dialect_from_settings(value: &serde_json::Value) -> Option<(Dialect, Option<String>)> {
    let name = value
        .get("cagara")
        .and_then(|c| c.get("dialect"))
        .or_else(|| value.get("dialect"))
        .and_then(|d| d.as_str())?;
    match cagara_sql::dialect(name) {
        Some(d) => Some((d, None)),
        None => Some((Dialect::Ansi, Some(name.to_string()))),
    }
}

/// `initialize` params → the configured dialect, ANSI when none is set.
fn dialect_from_initialize(params: &serde_json::Value) -> Dialect {
    let Some(options) = params.get("initializationOptions") else {
        return Dialect::Ansi;
    };
    match dialect_from_settings(options) {
        Some((d, _)) => d,
        None => Dialect::Ansi,
    }
}

fn publish(
    sender: &Sender<Message>,
    uri: Uri,
    ws: &Workspace,
    version: Option<i32>,
    dialect: Dialect,
) -> Res<()> {
    let diagnostics: Vec<Diagnostic> = analysis::diagnostics_with(ws, dialect)
        .into_iter()
        .map(|report| Diagnostic {
            range: report.range,
            severity: Some(match report.severity {
                cagara_hir::workspace::Severity::Error => DiagnosticSeverity::ERROR,
                cagara_hir::workspace::Severity::Warning => DiagnosticSeverity::WARNING,
            }),
            // The code is the diagnostic's *kind*, not a reading of its
            // message: rewording a message must not change what a client
            // filters or links on.
            code: Some(lsp_types::NumberOrString::String(
                analysis::diagnostic_code(report.kind).into(),
            )),
            source: Some("cagara".into()),
            message: report.message,
            ..Default::default()
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
    /// How long a test waits for the server to answer *at all*.
    ///
    /// This is a hang detector, not a performance assertion: the server runs on
    /// its own thread and the tests drive it over an in-memory connection, so the
    /// only question is "did it answer". Six hundred milliseconds was enough when
    /// this suite ran alone and not enough with other test binaries competing for
    /// the machine, which made the result depend on how the suite was invoked.
    const SERVER_BUDGET: Duration = Duration::from_millis(5_000);

    /// How long a test waits before concluding the server will publish *nothing*.
    ///
    /// A test that asserts silence has to spend this to make its point, so it is a
    /// floor on that test's cost rather than a hang detector — and the server
    /// publishes on the first analysis pass, so it does not need to be long.
    const SERVER_SILENCE: Duration = Duration::from_millis(600);
    use super::{
        analyse, apply_change, coalesce_changes, serve, Cancellations, LogMessage,
        LogMessageParams, MessageType, PublishDiagnostics, Work,
    };
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
    // `LogMessage::METHOD` is a method of this trait, and `lsp_server` has a
    // `Notification` of its own, hence the anonymous import.
    use lsp_types::notification::Notification as _;
    use lsp_types::{DidChangeTextDocumentParams, PublishDiagnosticsParams};
    use serde_json::{json, Value};
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

    /// The notification on its own, for the tests that drive the batch
    /// collector directly instead of a connection.
    fn notification(method: &str, params: Value) -> Notification {
        Notification {
            method: method.into(),
            params,
        }
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

    /// A running server, driven the way an editor drives it.
    ///
    /// The tests around this one each drive one capability or one bug. The
    /// question here is different: can a *client* reach each capability at all —
    /// advertised, dispatched, parameter-typed, and answered in the shape the
    /// protocol says? `analysis::*` has 56 tests that answer "does the analysis
    /// work"; this answers "does the editor get it", which is the only thing a
    /// user sees.
    struct Editor {
        client: Connection,
        thread: Option<std::thread::JoinHandle<Result<(), String>>>,
        next: i32,
        /// What `initialize` answered, so a test can assert the shape a client
        /// reads rather than the shape the server built. The two are not the
        /// same thing: a capability can be `Some` in Rust and still serialise
        /// to something a client interprets differently — a bare
        /// `TextDocumentSyncKind` is the number `2`, which says nothing about
        /// saving, and that is how the `didSave` handler became unreachable.
        caps: Value,
    }

    impl Editor {
        fn start() -> Self {
            Self::start_with(json!({ "capabilities": {} }))
        }

        /// A server started with these `initialize` params.
        ///
        /// The dialect comes in through `initializationOptions`, so a test that wants
        /// one has to send the real request rather than `start`'s minimum.
        fn start_with(params: Value) -> Self {
            let (server, client) = Connection::memory();
            let thread = std::thread::spawn(move || {
                serve(&server, "0.0.0 (test)").map_err(|e| e.to_string())
            });
            let mut ed = Self {
                client,
                thread: Some(thread),
                next: 1,
                caps: Value::Null,
            };
            ed.caps = ed.ask("initialize", params);
            ed.client
                .sender
                .send(notify("initialized", json!({})))
                .unwrap();
            ed
        }

        /// Ask and wait for the answer. A `MethodNotFound` comes back as an
        /// error object rather than a panic, so the caller can assert on it.
        fn ask(&mut self, method: &str, params: Value) -> Value {
            let id = self.next;
            self.next += 1;
            self.client
                .sender
                .send(request(id, method, params))
                .expect("send");
            let r = response(&self.client, id);
            match r.response_result {
                Ok(v) => v,
                Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
            }
        }

        /// Every notification the server sends within `quiet` of the last one.
        ///
        /// `ask` drops notifications while it waits for its response, which is right
        /// for a capability test and useless for the one thing the server sends without
        /// being asked: diagnostics. This collects them.
        /// Every message the server sends: waiting up to `budget` for the first
        /// one, then draining for a moment after the last.
        ///
        /// The two waits are different questions and one number cannot answer
        /// both. `budget` is "before I conclude it is not going to answer" — a
        /// hang detector, so generous. The drain is "after the last message,
        /// before I conclude there are no more" — the server writes a burst in one
        /// go, so a short one is plenty.
        fn notifications(&mut self, budget: Duration) -> Vec<Message> {
            let Ok(first) = self.client.receiver.recv_timeout(budget) else {
                return Vec::new();
            };
            let mut out = vec![first];
            const DRAIN: Duration = Duration::from_millis(600);
            while let Ok(m) = self.client.receiver.recv_timeout(DRAIN) {
                out.push(m);
            }
            out
        }

        /// The diagnostics published for `uri`, as the editor receives them.
        fn published(&mut self, uri: &str, quiet: Duration) -> Vec<Value> {
            let want = uri.to_string();
            let mut found = Vec::new();
            for m in self.notifications(quiet) {
                if let Message::Notification(n) = m {
                    if n.method != "textDocument/publishDiagnostics" {
                        continue;
                    }
                    if n.params["uri"] == json!(want) {
                        found = n.params["diagnostics"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                    }
                }
            }
            found
        }

        fn open(&mut self, uri: &str, text: &str) {
            self.client
                .sender
                .send(notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": uri, "languageId": "cagara", "version": 1, "text": text
                    } }),
                ))
                .expect("didOpen");
        }

        fn at(uri: &str, line: u32, character: u32) -> Value {
            json!({ "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character } })
        }

        fn close(mut self) {
            self.ask("shutdown", Value::Null);
            self.client
                .sender
                .send(notify("exit", Value::Null))
                .unwrap();
            if let Some(t) = self.thread.take() {
                t.join()
                    .expect("the server thread ended")
                    .expect("clean exit");
            }
        }
    }

    const DOC: &str = "users : query { id = int, name = string } = table \"p\" \"users\"\n\
                       adult = .id >= 18\n\
                       q = users & where adult & select { id = .id, who = .name }\n";
    const URI: &str = "file:///nonexistent/cagara-caps/main.cagara";

    /// Saving is advertised in a form a client can act on.
    ///
    /// `textDocumentSync` used to be the bare `TextDocumentSyncKind::INCREMENTAL`,
    /// which serialises as the number `2`. A number has no room to say anything
    /// about saving, and the protocol reads the absence of
    /// `textDocumentSyncOptions.save` as "this server does not want save
    /// notifications" — so no conforming client ever sent one. Measured against
    /// the shipped client: VS Code registers its `DidSaveTextDocumentFeature`
    /// only when that field is set, which made the `didSave` handler dead code
    /// and left the save path exercised by nothing but a test that sent the
    /// notification regardless of what had been advertised.
    ///
    /// The assertion is on the serialised answer, not on `ServerCapabilities`,
    /// because the two differing is exactly what went wrong: `Some(...)` in Rust
    /// is not the same as a shape the client understands.
    #[test]
    fn the_save_notification_is_advertised() {
        let ed = Editor::start();
        let sync = &ed.caps["capabilities"]["textDocumentSync"];
        assert_eq!(
            sync["change"],
            json!(2),
            "incremental sync is still what is advertised: {sync}"
        );
        assert_eq!(
            sync["openClose"],
            json!(true),
            "open and close are what the document map is built from: {sync}"
        );
        assert_eq!(
            sync["save"]["includeText"],
            json!(true),
            "a save must ask for the text, or the client may send only a URI: {sync}"
        );
        ed.close();
    }

    /// Every capability the server advertises answers through the protocol.
    ///
    /// A capability is advertised in `initialize` and dispatched in `request`;
    /// nothing checked that the two lists agree, so a method could be advertised
    /// and answer `MethodNotFound` (the editor greys out a feature that is
    /// "supported") or be dispatched and never advertised (the editor never asks).
    /// The list below is the advertised set, and each entry is asked for real.
    #[test]
    fn every_advertised_capability_answers() {
        let mut ed = Editor::start();
        ed.open(URI, DOC);

        let hover = ed.ask("textDocument/hover", Editor::at(URI, 1, 8));
        assert!(
            hover["contents"]["value"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "hover: {hover}"
        );

        // `adult` on line 1 is defined there and used on line 2.
        let def = ed.ask("textDocument/definition", Editor::at(URI, 2, 19));
        assert_eq!(
            def[0]["range"]["start"]["line"], 1,
            "definition of `adult`: {def}"
        );

        let refs = ed.ask(
            "textDocument/references",
            json!({ "textDocument": { "uri": URI },
                    "position": { "line": 1, "character": 2 },
                    "context": { "includeDeclaration": true } }),
        );
        assert!(
            refs.as_array().is_some_and(|a| a.len() >= 2),
            "a definition and its use: {refs}"
        );

        // `prepareRename` puts the editor into rename mode; the answer is the range
        // of the name, so the editor selects exactly what will change.
        let prep = ed.ask("textDocument/prepareRename", Editor::at(URI, 1, 2));
        assert_eq!(prep["start"]["line"], 1, "prepareRename on `adult`: {prep}");

        let ren = ed.ask(
            "textDocument/rename",
            json!({ "textDocument": { "uri": URI },
                    "position": { "line": 1, "character": 2 },
                    "newName": "grown_up" }),
        );
        let edits = ren["changes"][URI]
            .as_array()
            .unwrap_or_else(|| panic!("rename produced no edits: {ren}"));
        assert!(
            edits.iter().all(|e| e["newText"] == "grown_up"),
            "every edit writes the new name: {ren}"
        );
        assert!(
            edits.len() >= 2,
            "the definition and its use are both renamed: {ren}"
        );

        // `signatureHelp` says which argument the cursor is in. `DOC` has
        // `q = users & where adult & select { id = .id, who = .name }`.
        let sig = ed.ask("textDocument/signatureHelp", Editor::at(URI, 2, 12));
        assert!(
            sig["signatures"][0]["label"]
                .as_str()
                .is_some_and(|l| !l.is_empty()),
            "signatureHelp: {sig}"
        );

        // `workspace/symbol` searches every open file, which is the difference from
        // `documentSymbol`. `DOC` defines `adult`, `q` and `users`.
        let ws_syms = ed.ask("workspace/symbol", json!({ "query": "" }));
        let names: Vec<&str> = ws_syms
            .as_array()
            .unwrap_or_else(|| panic!("workspace/symbol: {ws_syms}"))
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(names.contains(&"adult"), "workspace/symbol: {ws_syms}");
        assert!(
            !names.contains(&"sum"),
            "the prelude's names are not the user's to jump to: {ws_syms}"
        );

        // `foldingRange` and `semanticTokens` are asked through the protocol too: an
        // advertised capability that only its analysis test reaches is advertised and
        // untested, which is what this test exists to prevent.
        let folds = ed.ask(
            "textDocument/foldingRange",
            json!({ "textDocument": { "uri": URI } }),
        );
        assert!(folds.as_array().is_some(), "foldingRange: {folds}");

        let toks = ed.ask(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": URI } }),
        );
        let data = toks["data"]
            .as_array()
            .unwrap_or_else(|| panic!("semanticTokens: {toks}"));
        assert!(!data.is_empty(), "semanticTokens: {toks}");
        // On the wire it is one flat array of five-number groups, not an array of
        // arrays — that is `SemanticToken`'s own serialization, and getting it wrong
        // here would mean testing a shape the editor never sees.
        assert_eq!(data.len() % 5, 0, "five numbers per token: {toks}");

        let hl = ed.ask("textDocument/documentHighlight", Editor::at(URI, 1, 2));
        assert!(
            hl.as_array().is_some_and(|a| !a.is_empty()),
            "highlights: {hl}"
        );

        let syms = ed.ask(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        );
        let names: Vec<&str> = syms
            .as_array()
            .expect("symbols")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(names.contains(&"q"), "{names:?}");

        let items = ed.ask("textDocument/completion", Editor::at(URI, 2, 11));
        assert!(
            items.as_array().is_some_and(|a| !a.is_empty()),
            "completion: {items}"
        );

        let hints = ed.ask(
            "textDocument/inlayHint",
            json!({ "textDocument": { "uri": URI },
                    "range": { "start": { "line": 0, "character": 0 },
                               "end": { "line": 3, "character": 0 } } }),
        );
        assert!(
            hints.as_array().is_some_and(|a| !a.is_empty()),
            "inlay hints: {hints}"
        );

        let actions = ed.ask(
            "textDocument/codeAction",
            json!({ "textDocument": { "uri": URI },
                    "range": { "start": { "line": 2, "character": 0 },
                               "end": { "line": 2, "character": 60 } },
                    "context": { "diagnostics": [] } }),
        );
        // This document has no fixable diagnostic, so the right answer is an
        // empty list — not an error, and not null.
        assert_eq!(actions, json!([]), "code actions: {actions}");

        // Formatting is asked last: it is the only capability that changes the
        // document, and every request above is about the text it was opened with.
        let edits = ed.ask(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": URI }, "options": { "tabSize": 2, "insertSpaces": true } }),
        );
        assert!(edits.as_array().is_some(), "formatting: {edits}");

        ed.close();
    }

    /// A file the lowering refuses is published as a diagnostic, at the stage.
    ///
    /// The checker accepts this program; `cagara-sql` does not. Before this the editor
    /// published nothing for it, so a file `cagara --only` rejects looked clean — and
    /// the author found out at build time, in a different window.
    #[test]
    fn a_lowering_refusal_is_published() {
        let mut ed = Editor::start();
        ed.open(
            URI,
            "t : query { id = int, name = string } = table \"m\" \"t\"\n\
             q = t & order [asc (.id + 1)] & select {.id} & distinct\n",
        );
        let ds = ed.published(URI, SERVER_BUDGET);
        assert_eq!(ds.len(), 1, "one diagnostic: {ds:?}");
        assert_eq!(ds[0]["severity"], json!(1), "an error: {ds:?}");
        assert!(
            ds[0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("cannot follow an `order`")),
            "{ds:?}"
        );
        // Line 1, not line 0: the range is the stage.
        assert_eq!(ds[0]["range"]["start"]["line"], json!(1), "{ds:?}");
        assert_eq!(ds[0]["source"], json!("cagara"), "{ds:?}");
        ed.close();
    }

    /// A target-specific refusal is *not* published, because the editor has no target.
    ///
    /// `fullJoin` is refused on MySQL and fine on ANSI. Publishing it would put a
    /// squiggle under a construct that is correct for the target the user chose and
    /// never told the server about.
    #[test]
    fn a_target_specific_refusal_is_not_published() {
        let mut ed = Editor::start();
        ed.open(
            URI,
            "a : query { id = int } = table \"m\" \"a\"\n\
             b : query { id = int } = table \"m\" \"b\"\n\
             q = a & fullJoin b (.<id == .>id)\n",
        );
        let ds = ed.published(URI, SERVER_BUDGET);
        assert!(
            !ds.iter().any(|d| d["message"]
                .as_str()
                .is_some_and(|m| m.contains("fullJoin"))),
            "`fullJoin` is fine on the dialect the editor asks about: {ds:?}"
        );
        ed.close();
    }

    /// A capability the server does not advertise is refused, not answered.
    ///
    /// Answering an unimplemented request with an empty result is worse than
    /// refusing it: an editor that asked for a rename would show "no changes"
    /// and the user would believe it.
    #[test]
    fn a_capability_that_is_not_advertised_is_refused() {
        let mut ed = Editor::start();
        ed.open(URI, DOC);
        // `textDocument/rename`, `workspace/symbol` and `textDocument/signatureHelp`
        // used to be in this list. All three are implemented now, so it holds only what
        // is still missing — the test is about the server refusing what it does not
        // advertise, and a method it *does* answer belongs in
        // `every_advertised_capability_answers` instead.
        //
        // `documentColor` stays missing on purpose: the language has no colour type, so
        // there is nothing to colour. Answering "no colours" for every document would be
        // what an editor already assumes when the capability is absent.
        // One left, so no loop: `documentColor` is the only advertised-capability
        // shape the server still does not answer.
        let r = ed.ask("textDocument/documentColor", Editor::at(URI, 1, 8));
        assert_eq!(
            r["error"]["code"],
            ErrorCode::MethodNotFound as i32,
            "documentColor must be refused: {r}"
        );
        ed.close();
    }

    /// A request for a document the server was never told about gets nothing.
    ///
    /// An editor can ask about a file it has not opened yet — a peeked
    /// definition, a workspace symbol — and the server must answer "nothing"
    /// rather than analyse whatever document it last selected.
    #[test]
    fn a_request_for_an_unopened_document_gets_nothing() {
        let mut ed = Editor::start();
        ed.open(URI, DOC);
        let other = "file:///nonexistent/cagara-caps/other.cagara";
        for (method, params) in [
            ("textDocument/hover", Editor::at(other, 0, 0)),
            (
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": other } }),
            ),
            ("textDocument/completion", Editor::at(other, 0, 0)),
        ] {
            let r = ed.ask(method, params);
            assert!(
                r.is_null(),
                "{method} for an unopened document must be null, got {r}"
            );
        }
        ed.close();
    }

    /// A definition in an imported buffer is found from the file that imports it.
    ///
    /// Both buffers are open and unsaved, so the import resolves through the
    /// workspace the server keeps rather than through the filesystem. That is the
    /// case an editor is in while a project is being edited, and the one the
    /// `select`/`rebuild` dance exists for.
    #[test]
    fn an_imported_definition_is_found_across_buffers() {
        let mut ed = Editor::start();
        let lib = "file:///nonexistent/cagara-caps/lib.cagara";
        ed.open(lib, "helper = 1\n");
        ed.open(URI, "import \"lib.cagara\" as lib\nq = lib.helper\n");
        // `helper` in `lib.helper` is at units 8..14 on line 1.
        let def = ed.ask("textDocument/definition", Editor::at(URI, 1, 10));
        let locs = def
            .as_array()
            .unwrap_or_else(|| panic!("definition: {def}"));
        assert_eq!(locs.len(), 1, "{def}");
        assert!(
            locs[0]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with("lib.cagara")),
            "the definition is in the imported file: {def}"
        );
        assert_eq!(locs[0]["range"]["start"]["line"], 0, "{def}");
        ed.close();
    }

    /// A definition used from another file lists that use.
    ///
    /// `textDocument/documentHighlight` is defined per document, so `highlights`
    /// stays root-only on purpose. A *reference* is not: an editor's "find all
    /// references" that listed only the file it was asked from would not error, it
    /// would show a short list — and the user would conclude the definition is
    /// unused. The locations have to carry their own URIs for the same reason: a
    /// link naming the requesting document opens the wrong file at a plausible
    /// position.
    #[test]
    fn references_cross_an_import() {
        let mut ed = Editor::start();
        let lib = "file:///nonexistent/cagara-caps/lib.cagara";
        ed.open(lib, "helper = 1\n");
        ed.open(URI, "import \"lib.cagara\" as lib\nq = lib.helper\n");
        let refs = ed.ask(
            "textDocument/references",
            json!({ "textDocument": { "uri": URI },
                    "position": { "line": 1, "character": 9 },
                    "context": { "includeDeclaration": true } }),
        );
        let locs = refs
            .as_array()
            .unwrap_or_else(|| panic!("references: {refs}"));
        assert_eq!(locs.len(), 2, "the definition and the use: {refs}");
        let files: Vec<&str> = locs
            .iter()
            .filter_map(|l| l["uri"].as_str())
            .map(|u| {
                if u.ends_with("lib.cagara") {
                    "lib"
                } else {
                    "main"
                }
            })
            .collect();
        assert_eq!(files, ["lib", "main"], "one in each file: {refs}");
        ed.close();
    }

    /// A position counts UTF-16 units, all the way through the protocol.
    ///
    /// The protocol measures `character` in UTF-16 code units, not bytes and not
    /// characters. An emoji is one character and two units, so a name after one sits
    /// one unit further right than a character count would put it — and every request
    /// that takes a position has to agree. A request that lands one unit off does not
    /// error: it answers about a *different* name, or about none, which reads as a
    /// bug in the analysis rather than in the mapping.
    ///
    /// The document below puts `q` after an emoji on the same line on purpose. With
    /// characters instead of units it would be found at 20 rather than 21.
    #[test]
    fn a_position_is_measured_in_utf16_units() {
        let mut ed = Editor::start();
        let text = "q = 1\nr = \"🎉\" ++ toString q\n";
        ed.open(URI, text);
        // `r = "🎉" ++ toString q` — in UTF-16 units the name starts at 21.
        let def = ed.ask("textDocument/definition", Editor::at(URI, 1, 21));
        assert_eq!(
            def[0]["range"]["start"]["line"], 0,
            "definition of `q` at the UTF-16 column: {def}"
        );
        // A character count would put it at 20, which is the space before it.
        // A character count would put the name at 20, which is the space before it.
        let off = ed.ask("textDocument/definition", Editor::at(URI, 1, 20));
        assert!(off.is_null(), "unit 20 is the space, not the name: {off}");
        ed.close();
    }

    /// Formatting a half-broken document formats what it can and leaves the rest.
    ///
    /// This is the one place the server and `cagara fmt` deliberately disagree, and
    /// the reason is what happens to the result. The CLI writes to disk, so a file
    /// it cannot fully understand is reported and left alone — rewriting someone's
    /// source from a guessed parse is not recoverable. The server hands edits to a
    /// buffer the user is *mid-keystroke* in, and refusing everything until the file
    /// parses means format-on-save silently does nothing all day. So it formats the
    /// definitions that parse and leaves the broken one as written, which is what
    /// `analysis::format`'s contract says.
    ///
    /// Pinned because it looks like a bug from either side: a reader who knows the
    /// CLI expects a refusal, and a reader who knows the server expects no partial
    /// edits.
    #[test]
    fn formatting_a_half_broken_document_formats_only_what_parses() {
        let mut ed = Editor::start();
        ed.open(URI, "q=1\nr = (\n");
        let r = ed.ask(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": URI }, "options": { "tabSize": 2, "insertSpaces": true } }),
        );
        let edits = r.as_array().unwrap_or_else(|| panic!("formatting: {r}"));
        assert_eq!(edits.len(), 1, "one edit for the one formattable line: {r}");
        assert_eq!(edits[0]["newText"], "q = 1\n", "{r}");
        // The broken line is outside every edit.
        assert_eq!(edits[0]["range"]["end"]["line"], 1, "{r}");
        ed.close();
    }

    /// A document that is entirely unparseable gets no edits, not an error.
    ///
    /// `cagara_fmt::format` succeeds for such a file and returns the text it
    /// produced with the parse errors beside it, so the server has edits to send
    /// and must not turn a mid-keystroke buffer into an `InternalError` — clients
    /// surface that as "the language server crashed".
    #[test]
    fn formatting_an_unparseable_document_is_not_an_internal_error() {
        let mut ed = Editor::start();
        ed.open(URI, "q = (\n");
        let r = ed.ask(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": URI }, "options": { "tabSize": 2, "insertSpaces": true } }),
        );
        assert!(
            r.is_null() || r.as_array().is_some_and(|a| a.is_empty()),
            "nothing to format yet: {r}"
        );
        ed.close();
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
    /// See `tests::notifications` for why the two waits differ.
    fn messages(client: &Connection, budget: Duration) -> Vec<String> {
        let Ok(first) = client.receiver.recv_timeout(budget) else {
            return Vec::new();
        };
        let mut pending = vec![first];
        while let Ok(message) = client.receiver.recv_timeout(Duration::from_millis(600)) {
            pending.push(message);
        }
        let mut out = Vec::new();
        for message in pending {
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

    /// Run one burst through the analysis thread, and report what an editor
    /// would be left with: the document's symbol names, the diagnostics of the
    /// last publish, and how many publishes the burst produced.
    ///
    /// The work is queued before the thread starts, so `next_batch` takes all
    /// of it: a burst is one batch however fast the analysis is. That is what
    /// makes a test of burst behaviour deterministic instead of a race against
    /// the analysis thread draining the queue between two keystrokes.
    fn burst_outcome(queued: Vec<Notification>, uri: &str) -> (Vec<String>, Vec<String>, usize) {
        let (work, jobs) = crossbeam_channel::unbounded::<Work>();
        let (out, sent) = crossbeam_channel::unbounded::<Message>();
        for n in queued {
            work.send(Work::Notify(n)).expect("queue");
        }
        work.send(Work::Request {
            id: RequestId::from(99),
            method: "textDocument/documentSymbol".into(),
            params: json!({ "textDocument": { "uri": uri } }),
        })
        .expect("queue");
        let t = std::thread::spawn({
            let cancelled = std::sync::Arc::new(Cancellations::default());
            move || analyse(jobs, out, cancelled, cagara_sql::Dialect::Ansi)
        });
        // The answer arrives as a response message, like every other answer
        // now that the analysis thread replies directly. Everything the burst
        // published arrives on the same channel before it, so it is collected
        // on the way.
        let mut publishes = 0;
        let mut diagnostics = Vec::new();
        let symbols = loop {
            let m = sent
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|e| panic!("no documentSymbol answer: {e}"));
            let Message::Response(r) = m else {
                if let Message::Notification(n) = m {
                    if n.method != PublishDiagnostics::METHOD {
                        continue;
                    }
                    let p: PublishDiagnosticsParams =
                        serde_json::from_value(n.params).expect("publish");
                    if p.uri.as_str() != uri {
                        continue;
                    }
                    publishes += 1;
                    diagnostics = p.diagnostics.iter().map(|d| d.message.clone()).collect();
                }
                continue;
            };
            assert_eq!(r.id, RequestId::from(99), "the answer to the request");
            match r.response_result {
                Ok(v) => break v,
                Err(e) => panic!("documentSymbol failed: {}", e.message),
            }
        };
        let names: Vec<String> = symbols
            .as_array()
            .unwrap_or_else(|| panic!("symbols: {symbols}"))
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_string))
            .collect();
        // Closing the work channel is what ends the analysis thread.
        drop(work);
        t.join().expect("the analysis thread ended");
        (names, diagnostics, publishes)
    }

    /// A burst of ranged changes is one notification that reproduces exactly
    /// the text the client sent.
    ///
    /// Each range is written against the text the change before it produced —
    /// that is what the protocol means — so the edits only line up when all of
    /// them are applied, in order. The collector used to keep just the last
    /// `didChange` of a document, and the second range here then points past
    /// the end of the un-edited line: the rename disappeared from the document
    /// and the user's first keystrokes were checked as if they never happened.
    #[test]
    fn a_burst_of_ranged_changes_coalesces_without_losing_an_edit() {
        let original = "alpha = 1\nzeta = 2\n";
        // `alpha` -> `alphabet` (line 0, units 0..5), then the `1` that the
        // longer name pushed to unit 11 -> `42`. The second range only exists
        // in the text the first change produced.
        let first = notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": URI, "version": 2 },
                "contentChanges": [{
                    "range": { "start": { "line": 0, "character": 0 },
                               "end": { "line": 0, "character": 5 } },
                    "text": "alphabet"
                }]
            }),
        );
        let second = notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": URI, "version": 3 },
                "contentChanges": [{
                    "range": { "start": { "line": 0, "character": 11 },
                               "end": { "line": 0, "character": 12 } },
                    "text": "42"
                }]
            }),
        );

        let batch = coalesce_changes(vec![first, second]);
        assert_eq!(batch.len(), 1, "a burst is one analysis, not one per edit");
        let params: DidChangeTextDocumentParams =
            serde_json::from_value(batch[0].params.clone()).expect("a didChange");
        assert_eq!(
            params.text_document.version, 3,
            "the newest version is the one diagnostics echo back"
        );
        let mut text = original.to_string();
        for change in params.content_changes {
            text = match change.range {
                None => change.text,
                Some(range) => apply_change(&text, range, &change.text),
            };
        }
        assert_eq!(text, "alphabet = 42\nzeta = 2\n", "every edit, in order");
    }

    /// A save of the document splits a burst; another document's change does
    /// not.
    ///
    /// A save can replace the buffer with the text the client wrote to disk, so
    /// a change before it and a change after it are written against different
    /// texts and must not be merged across it. A change for a different
    /// document touches nothing here, so the run carries on through it.
    #[test]
    fn only_a_barrier_for_the_same_document_splits_a_burst() {
        let change = |uri: &str, version: i32, text: &str| {
            notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{
                        "range": { "start": { "line": 0, "character": 0 },
                                   "end": { "line": 0, "character": 0 } },
                        "text": text
                    }]
                }),
            )
        };
        let other = "file:///nonexistent/cagara-caps/other.cagara";

        let merged = coalesce_changes(vec![
            change(URI, 2, "a"),
            change(other, 2, "b"),
            change(URI, 3, "c"),
        ]);
        assert_eq!(
            merged.len(),
            2,
            "one notification for this document, one for the other"
        );
        let p: DidChangeTextDocumentParams =
            serde_json::from_value(merged[0].params.clone()).expect("a didChange");
        assert_eq!(p.text_document.version, 3);
        assert_eq!(
            p.content_changes.len(),
            2,
            "both of this document's changes survive, in order"
        );

        let saved = coalesce_changes(vec![
            change(URI, 2, "a"),
            notification(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": URI }, "text": "alpha = 1\n" }),
            ),
            change(URI, 3, "b"),
        ]);
        assert_eq!(saved.len(), 3, "a save is not merged over");
        let before: DidChangeTextDocumentParams =
            serde_json::from_value(saved[0].params.clone()).expect("a didChange");
        let after: DidChangeTextDocumentParams =
            serde_json::from_value(saved[2].params.clone()).expect("a didChange");
        assert_eq!(before.content_changes.len(), 1);
        assert_eq!(after.content_changes.len(), 1);
        assert_eq!(before.text_document.version, 2);
        assert_eq!(after.text_document.version, 3);
    }

    /// A burst of ranged edits leaves the document exactly as opening the
    /// final text would: same symbols, same diagnostics, and one analysis.
    ///
    /// This is the end-to-end shape of the bug the coalescing fixes. The first
    /// change renames `alpha`, the second uses the new name from another line.
    /// Keeping only the second change checked `bravo` in a document that never
    /// defines it, so the burst and the fresh open disagreed on both the
    /// outline and the errors.
    #[test]
    fn a_burst_of_ranged_edits_matches_opening_the_final_text() {
        let opened = notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": URI, "languageId": "cagara", "version": 1,
                "text": "alpha = 1\nzeta = 2\n"
            } }),
        );
        let first = notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": URI, "version": 2 },
                "contentChanges": [{
                    "range": { "start": { "line": 0, "character": 0 },
                               "end": { "line": 0, "character": 5 } },
                    "text": "bravo"
                }]
            }),
        );
        let second = notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": URI, "version": 3 },
                "contentChanges": [{
                    "range": { "start": { "line": 1, "character": 7 },
                               "end": { "line": 1, "character": 8 } },
                    "text": "bravo + 1"
                }]
            }),
        );
        let (names, diagnostics, publishes) = burst_outcome(vec![opened, first, second], URI);
        assert_eq!(
            names,
            ["bravo", "zeta"].map(str::to_string),
            "both edits were applied, in order: {names:?}"
        );
        assert_eq!(
            publishes, 2,
            "one publish for the open and one for the burst — not one per edit"
        );

        // The same document, opened already holding the text the burst ended
        // with. If the burst was checked as the document the client sent, the
        // two agree on everything an editor shows.
        let fresh = notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": URI, "languageId": "cagara", "version": 3,
                "text": "bravo = 1\nzeta = bravo + 1\n"
            } }),
        );
        let (fresh_names, fresh_diagnostics, _) = burst_outcome(vec![fresh], URI);
        assert_eq!(
            names, fresh_names,
            "a burst leaves the same outline as opening the final text"
        );
        assert_eq!(
            diagnostics, fresh_diagnostics,
            "and the same diagnostics: the burst was checked as the final text"
        );
    }

    /// A request the client cancels is answered `RequestCancelled`, not with a
    /// result it no longer wants.
    ///
    /// The cancel is sent immediately before the request it names, so it is the
    /// first of the two the router reads, and the answer is decided before any
    /// analysis happens — no part of this test depends on which thread wins.
    /// It is also the case the blocking router could not serve at all: it
    /// waited for the answer to one request before reading the next message, so
    /// a cancel was still unread when the result it was meant to stop had
    /// already been sent.
    ///
    /// The protocol wants a response to every request, so the cancelled one is
    /// answered rather than dropped; the editor sees the code and discards it
    /// instead of drawing a stale hover.
    #[test]
    fn a_cancelled_request_is_answered_with_request_cancelled() {
        let mut ed = Editor::start();
        ed.open(URI, DOC);
        let _ = ed.published(URI, SERVER_BUDGET);

        let id = ed.next;
        ed.next += 1;
        ed.client
            .sender
            .send(notify("$/cancelRequest", json!({ "id": id })))
            .expect("cancel");
        ed.client
            .sender
            .send(request(
                id,
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": URI } }),
            ))
            .expect("request");
        let r = response(&ed.client, id);
        let e = r
            .response_result
            .expect_err("a cancelled request must not be answered with a result");
        assert_eq!(
            e.code,
            ErrorCode::RequestCanceled as i32,
            "RequestCancelled, -32800: {}",
            e.message
        );
        // Pinned as the number on the wire, not just the enum the crate spells
        // with one `l`: an editor compares the protocol's code.
        assert_eq!(e.code, -32800);

        // The cancel is for one request, not for the session: the same question
        // asked again is answered.
        let syms = ed.ask(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        );
        assert!(
            syms.as_array().is_some_and(|a| !a.is_empty()),
            "symbols after a cancel: {syms}"
        );
        ed.close();
    }

    /// A burst of requests is answered exactly once each.
    ///
    /// Requests used to be answered one at a time, each waiting for the answer
    /// to the one before it. Now the router queues them and keeps reading, so
    /// the answers come from the analysis thread; the risk that buys is a
    /// request answered twice, or not at all, and this is the test for it.
    #[test]
    fn a_burst_of_requests_is_each_answered_exactly_once() {
        let mut ed = Editor::start();
        ed.open(URI, DOC);
        let _ = ed.published(URI, SERVER_BUDGET);

        let ids: Vec<i32> = (100..108).collect();
        for id in &ids {
            ed.client
                .sender
                .send(request(
                    *id,
                    "textDocument/documentSymbol",
                    json!({ "textDocument": { "uri": URI } }),
                ))
                .expect("request");
        }
        let mut answered: Vec<i32> = Vec::new();
        while answered.len() < ids.len() {
            match ed.client.receiver.recv_timeout(Duration::from_secs(60)) {
                Ok(Message::Response(r)) => {
                    let id = r.id.to_string().parse().expect("a numeric id");
                    answered.push(id);
                }
                // The publishes of the open and of the burst arrive between the
                // answers and are not what this test is about.
                Ok(_) => {}
                Err(e) => panic!(
                    "{} of {} requests answered before the stream went quiet: {e}",
                    answered.len(),
                    ids.len()
                ),
            }
        }
        answered.sort();
        assert_eq!(answered, ids, "one answer per request");

        // Nothing follows: a second answer to any of them would show up here.
        let extra: Vec<i32> = ed
            .notifications(Duration::from_millis(400))
            .into_iter()
            .filter_map(|m| match m {
                Message::Response(r) => r.id.to_string().parse().ok(),
                _ => None,
            })
            .collect();
        assert!(
            extra.is_empty(),
            "a request was answered twice: {extra:?} after {ids:?}"
        );
        ed.close();
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
        // An assertion of silence: it has to spend the wait to make its point.
        // The diagnostics this test then looks for *do* arrive, so they get the
        // generous budget.
        let quiet = SERVER_BUDGET;
        assert!(
            messages(&client, SERVER_SILENCE).is_empty(),
            "the file checks"
        );

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

    /// The symbol names in a `documentSymbol` answer, in document order.
    fn symbol_names(symbols: &Value) -> Vec<String> {
        symbols
            .as_array()
            .unwrap_or_else(|| panic!("symbols: {symbols}"))
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_string))
            .collect()
    }

    /// A save carrying different text resynchronises the workspace.
    ///
    /// `didSave` assigned the server's copy of the document and stopped there,
    /// so the *workspace* — the text the analysis actually reads — kept the
    /// pre-save text. Two copies of one document disagreeing is not a state a
    /// later reader can reason about: `documentSymbol` answers from the
    /// workspace, and a rebuild takes its buffers from the other copy.
    ///
    /// This is the resynchronisation path rather than the common one. A save
    /// normally writes the buffer, so the text matches and nothing is re-parsed;
    /// `a_save_publishes_only_the_documents_that_changed` pins that case, and
    /// the comparison here is what keeps the two from being the same code path.
    #[test]
    fn a_save_resynchronises_the_document_the_analysis_reads() {
        let mut ed = Editor::start();
        ed.open(URI, "alpha = 1\nq = 2\n");
        let opened = ed.ask(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        );
        assert_eq!(symbol_names(&opened), ["alpha", "q"], "the opened text");

        // The client saved text the server has not been told about. `includeText`
        // is what lets it send that, which is why the capability asks for it.
        ed.client
            .sender
            .send(notify(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": URI }, "text": "bravo = 1\nzeta = 2\n" }),
            ))
            .unwrap();

        let saved = ed.ask(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        );
        assert_eq!(
            symbol_names(&saved),
            ["bravo", "zeta"],
            "the workspace must hold the text that was saved, not the text before it"
        );
        ed.close();
    }

    /// A save without text is still a save.
    ///
    /// `includeText` is what the server *asks* for; a client may ignore it and
    /// send only the URI. The handler must then fall back to the file rather
    /// than treat the notification as malformed, so this asserts the session
    /// survives it and the document is unchanged.
    #[test]
    fn a_save_without_text_is_harmless() {
        let mut ed = Editor::start();
        ed.open(URI, "alpha = 1\nq = 2\n");
        ed.client
            .sender
            .send(notify(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": URI } }),
            ))
            .unwrap();

        let after = ed.ask(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        );
        assert_eq!(
            symbol_names(&after),
            ["alpha", "q"],
            "a save with no text must leave the buffer alone"
        );
        ed.close();
    }

    /// The dialect a client configures reaches the diagnostics it is shown.
    ///
    /// This is the setting the finding asked for, end to end through the protocol: a
    /// `groups` window frame is refused for MySQL and T-SQL (both document
    /// `{ ROWS | RANGE }`) and fine for DuckDB. So the same document must gain a
    /// diagnostic when the server is initialized for MySQL and lose it when it is
    /// re-initialized for DuckDB — which only happens if `cagara.dialect` is read and
    /// threaded all the way into `cagara_sql::Options`.
    ///
    /// The negative control is built in: with no setting, the fallback is ANSI, which
    /// has every frame unit, so the document is clean. A server that ignored the
    /// setting would pass the ANSI case and fail the MySQL one.
    #[test]
    fn a_configured_dialect_reaches_the_diagnostics() {
        const FRAME: &str = "f : query { k = int, n = int } = table \"m\" \"f\"\n\
                            q = f & select { s = over { order = [asc .k], \
                            frame = groups (preceding 1) currentRow } (sum .n) }\n";

        // No setting: ANSI, which reads `GROUPS`, so nothing is reported.
        let mut ed = Editor::start();
        ed.open(URI, FRAME);
        let none = ed.published(URI, Duration::from_secs(2));
        assert!(
            !none.iter().any(|d| d["message"]
                .as_str()
                .is_some_and(|m| m.contains("`groups` window frame"))),
            "with no dialect the fallback is ANSI, which has `GROUPS`: {none:?}"
        );
        ed.close();

        // Initialized for MySQL, which does not: the refusal is published.
        let mut ed = Editor::start_with(json!({
            "capabilities": {},
            "initializationOptions": { "cagara": { "dialect": "mysql" } },
        }));
        ed.open(URI, FRAME);
        let mysql = ed.published(URI, Duration::from_secs(2));
        assert!(
            mysql.iter().any(|d| d["message"]
                .as_str()
                .is_some_and(|m| m.contains("`groups` window frame"))),
            "a MySQL window must show the frame refusal: {mysql:?}"
        );
        // And the code is the structured kind, not a reading of the message.
        let refused = mysql
            .iter()
            .find(|d| {
                d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("`groups` window frame"))
            })
            .expect("the refusal");
        assert_eq!(refused["code"], json!("lowering"), "{refused:?}");

        // A settings change re-publishes: the same open document, now for DuckDB,
        // which reads `GROUPS`, so the diagnostic goes away without reopening it.
        ed.client
            .sender
            .send(notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": { "cagara": { "dialect": "duckdb" } } }),
            ))
            .unwrap();
        let duckdb = ed.published(URI, Duration::from_secs(2));
        assert!(
            duckdb.is_empty()
                || !duckdb.iter().any(|d| d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("`groups` window frame"))),
            "after switching to duckdb the frame is readable: {duckdb:?}"
        );
        ed.close();
    }

    /// A misspelled dialect warns and falls back, rather than killing the session.
    ///
    /// A configuration value is not program text: getting it wrong must not stop the
    /// editor working. The warning says what the known names are, and diagnostics keep
    /// coming on the fallback so the user sees why nothing else changed.
    #[test]
    fn an_unknown_dialect_warns_and_falls_back() {
        let mut ed = Editor::start_with(json!({
            "capabilities": {},
            "initializationOptions": { "cagara": { "dialect": "mysql" } },
        }));
        ed.open(URI, "q = 1\n");
        let _ = ed.published(URI, Duration::from_secs(2));
        ed.client
            .sender
            .send(notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": { "cagara": { "dialect": "maysql" } } }),
            ))
            .unwrap();
        let warnings: Vec<String> = ed
            .notifications(Duration::from_secs(2))
            .into_iter()
            .filter_map(|m| match m {
                Message::Notification(n) if n.method == "window/showMessage" => {
                    n.params["message"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("maysql")),
            "the warning must name the value that was not understood: {warnings:?}"
        );
        ed.close();
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
            published(&client, SERVER_BUDGET),
            vec!["a.cagara", "b.cagara", "lib.cagara"]
        );

        let quiet = SERVER_BUDGET;

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
        let quiet = SERVER_BUDGET;
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

        // A fix comes from the diagnostic itself. The two mechanical ones are a
        // bare sort key and a name a "did you mean" resolved; the join-rename fix
        // is gone, because the compiler renames the column instead of warning
        // about it.
        client
            .sender
            .send(notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{
                        "text": "t : query { a = int } = table \"s\" \"t\"\n\
                                 unsorted = t & order [.a]\n\
                                 bad : intt = 1\n"
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
        let titles: Vec<&str> = actions
            .iter()
            .filter_map(|a| a.get("title").and_then(|t| t.as_str()))
            .collect();
        // The bare sort key offers both directions, and the unknown type offers
        // the name it was reaching for.
        assert!(
            titles.iter().any(|t| t.contains("`asc`"))
                && titles.iter().any(|t| t.contains("`desc`")),
            "the bare sort key has a fix: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.contains("Rename to `int`")),
            "the unknown type has a \"did you mean\" fix: {titles:?}"
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
