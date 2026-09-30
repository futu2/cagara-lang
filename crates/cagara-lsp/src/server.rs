//! The language server over stdio (`cagara lsp`). Full-text sync; each open
//! document is the root of its own workspace, updated with `set_source`
//! (reloaded when its imports change). Imported files are read from disk.

use crate::{analysis, uri};
use cagara_hir::workspace::Workspace;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentHighlightRequest, DocumentSymbolRequest, Formatting, GotoDefinition,
    HoverRequest, References, Request as _,
};
use lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, HoverProviderCapability, Location, MarkupContent, MarkupKind, OneOf,
    PublishDiagnosticsParams, ReferenceParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use std::collections::HashMap;
use std::error::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

pub type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct Doc {
    path: PathBuf,
    /// Last text the client sent, to rebuild `ws` after a panic.
    text: String,
    ws: Workspace,
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
pub fn run() -> Res<()> {
    let (conn, io) = Connection::stdio();
    serve(&conn)?;
    // The writer thread ends when every sender is gone.
    drop(conn);
    io.join()?;
    Ok(())
}

/// Initialize, then handle messages until shutdown or disconnect.
fn serve(conn: &Connection) -> Res<()> {
    let caps = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    conn.initialize(serde_json::to_value(caps)?)?;
    // Keyed by the URI string: `Uri` has interior mutability.
    let mut docs: HashMap<String, Doc> = HashMap::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    break;
                }
                let Request { id, method, params } = req;
                let r = catch_unwind(AssertUnwindSafe(|| request(&mut docs, &method, params)));
                let resp = match r {
                    Ok(Ok(v)) => Response::new_ok(id, v),
                    Ok(Err((code, msg))) => Response::new_err(id, code as i32, msg),
                    Err(p) => {
                        rebuild(&mut docs);
                        let msg = format!("internal error in {method}: {}", panic_message(&*p));
                        Response::new_err(id, ErrorCode::InternalError as i32, msg)
                    }
                };
                conn.sender.send(Message::Response(resp))?;
            }
            Message::Notification(n) => {
                let method = n.method.clone();
                match catch_unwind(AssertUnwindSafe(|| notification(conn, &mut docs, n))) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("cagara lsp: {method}: {e}"),
                    Err(p) => {
                        rebuild(&mut docs);
                        eprintln!(
                            "cagara lsp: internal error in {method}: {}",
                            panic_message(&*p)
                        );
                    }
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// A panic may have left a workspace half-updated (completion edits the
/// text in place); start each one over from what the client last sent.
fn rebuild(docs: &mut HashMap<String, Doc>) {
    for d in docs.values_mut() {
        if let Ok(ws) = catch_unwind(|| Workspace::open_with(&d.path, d.text.clone())) {
            d.ws = ws;
        }
    }
}

fn request(
    docs: &mut HashMap<String, Doc>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Fail> {
    match method {
        HoverRequest::METHOD => {
            let p: HoverParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position_params;
            let h = docs
                .get(tp.text_document.uri.as_str())
                .and_then(|d| analysis::hover(&d.ws, tp.position));
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
            let locs: Vec<Location> = docs
                .get(tp.text_document.uri.as_str())
                .map(|d| analysis::definition(&d.ws, tp.position))
                .unwrap_or_default()
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
            let uri = tp.text_document.uri;
            let locs: Option<Vec<Location>> = docs.get(uri.as_str()).map(|d| {
                analysis::references(&d.ws, tp.position, p.context.include_declaration)
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
            let hs: Option<Vec<DocumentHighlight>> =
                docs.get(tp.text_document.uri.as_str()).map(|d| {
                    analysis::highlights(&d.ws, tp.position)
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
            let ss = docs
                .get(p.text_document.uri.as_str())
                .map(|d| analysis::symbols(&d.ws));
            json(serde_json::to_value(ss.map(DocumentSymbolResponse::Nested)))
        }
        Completion::METHOD => {
            let p: CompletionParams = serde_json::from_value(params).map_err(bad_params)?;
            let tp = p.text_document_position;
            let items = docs
                .get_mut(tp.text_document.uri.as_str())
                .map(|d| analysis::completion(&mut d.ws, tp.position));
            json(serde_json::to_value(items.map(CompletionResponse::Array)))
        }
        Formatting::METHOD => {
            let p: DocumentFormattingParams = serde_json::from_value(params).map_err(bad_params)?;
            match docs.get(p.text_document.uri.as_str()) {
                None => Ok(serde_json::Value::Null),
                Some(d) => match analysis::format(&d.ws) {
                    Ok(edits) => json(serde_json::to_value(edits)),
                    Err(e) => Err((ErrorCode::InternalError, e.to_string())),
                },
            }
        }
        _ => Err((ErrorCode::MethodNotFound, format!("unsupported: {method}"))),
    }
}

fn notification(conn: &Connection, docs: &mut HashMap<String, Doc>, n: Notification) -> Res<()> {
    match n.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let p: lsp_types::DidOpenTextDocumentParams = serde_json::from_value(n.params)?;
            let Some(path) = uri::to_path(&p.text_document.uri) else {
                return Ok(());
            };
            let path = path.canonicalize().unwrap_or(path);
            let text = p.text_document.text;
            let ws = Workspace::open_with(&path, text.clone());
            let doc = Doc { path, text, ws };
            publish(conn, p.text_document.uri.clone(), &doc)?;
            docs.insert(p.text_document.uri.as_str().to_string(), doc);
        }
        DidChangeTextDocument::METHOD => {
            let p: lsp_types::DidChangeTextDocumentParams = serde_json::from_value(n.params)?;
            let (Some(doc), Some(change)) = (
                docs.get_mut(p.text_document.uri.as_str()),
                p.content_changes.into_iter().last(),
            ) else {
                return Ok(());
            };
            doc.text = change.text.clone();
            let root = doc.ws.root;
            if !doc.ws.set_source(root, change.text.clone()) {
                // Imports changed: loading files is outside salsa.
                doc.ws = Workspace::open_with(&doc.path, change.text);
            }
            publish(conn, p.text_document.uri, doc)?;
        }
        DidCloseTextDocument::METHOD => {
            let p: lsp_types::DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
            docs.remove(p.text_document.uri.as_str());
            let params = PublishDiagnosticsParams {
                uri: p.text_document.uri,
                diagnostics: vec![],
                version: None,
            };
            conn.sender.send(Message::Notification(Notification::new(
                PublishDiagnostics::METHOD.into(),
                params,
            )))?;
        }
        _ => {}
    }
    Ok(())
}

fn publish(conn: &Connection, uri: Uri, doc: &Doc) -> Res<()> {
    let diagnostics = analysis::diagnostics(&doc.ws)
        .into_iter()
        .map(|(range, message)| Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("cagara".into()),
            message,
            ..Default::default()
        })
        .collect();
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    conn.sender.send(Message::Notification(Notification::new(
        PublishDiagnostics::METHOD.into(),
        params,
    )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::serve;
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
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

    #[test]
    fn malformed_messages_do_not_end_the_session() {
        let (server, client) = Connection::memory();
        let t = std::thread::spawn(move || serve(&server).map_err(|e| e.to_string()));
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
}
