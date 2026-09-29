//! The language server over stdio (`cagara lsp`). Full-text sync; each open
//! document is the root of its own workspace, updated with `set_source`
//! (reloaded when its imports change). Imported files are read from disk.

use crate::{analysis, uri};
use cagara_hir::workspace::Workspace;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentHighlightRequest, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest,
    References, Request as _,
};
use lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity, DocumentHighlight,
    DocumentFormattingParams, DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability, Location, MarkupContent,
    MarkupKind, OneOf, PublishDiagnosticsParams, ReferenceParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;

pub type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct Doc {
    path: PathBuf,
    ws: Workspace,
}

/// Serve LSP over stdin / stdout until the client shuts the server down.
pub fn run() -> Res<()> {
    let (conn, io) = Connection::stdio();
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
                let result = if method == HoverRequest::METHOD {
                    let p: HoverParams = serde_json::from_value(params)?;
                    let tp = p.text_document_position_params;
                    let h = docs.get(tp.text_document.uri.as_str()).and_then(|d| analysis::hover(&d.ws, tp.position));
                    serde_json::to_value(h.map(|value| Hover {
                        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
                        range: None,
                    }))?
                } else if method == GotoDefinition::METHOD {
                    let p: GotoDefinitionParams = serde_json::from_value(params)?;
                    let tp = p.text_document_position_params;
                    let locs: Vec<Location> = docs
                        .get(tp.text_document.uri.as_str())
                        .map(|d| analysis::definition(&d.ws, tp.position))
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|(path, range)| Some(Location { uri: uri::from_path(&path)?, range }))
                        .collect();
                    serde_json::to_value((!locs.is_empty()).then_some(GotoDefinitionResponse::Array(locs)))?
                } else if method == References::METHOD {
                    let p: ReferenceParams = serde_json::from_value(params)?;
                    let tp = p.text_document_position;
                    let uri = tp.text_document.uri;
                    let locs: Option<Vec<Location>> = docs.get(uri.as_str()).map(|d| {
                        analysis::references(&d.ws, tp.position, p.context.include_declaration)
                            .into_iter()
                            .map(|range| Location { uri: uri.clone(), range })
                            .collect()
                    });
                    serde_json::to_value(locs)?
                } else if method == DocumentHighlightRequest::METHOD {
                    let p: DocumentHighlightParams = serde_json::from_value(params)?;
                    let tp = p.text_document_position_params;
                    let hs: Option<Vec<DocumentHighlight>> = docs.get(tp.text_document.uri.as_str()).map(|d| {
                        analysis::highlights(&d.ws, tp.position)
                            .into_iter()
                            .map(|(range, kind)| DocumentHighlight { range, kind: Some(kind) })
                            .collect()
                    });
                    serde_json::to_value(hs)?
                } else if method == DocumentSymbolRequest::METHOD {
                    let p: DocumentSymbolParams = serde_json::from_value(params)?;
                    let ss = docs.get(p.text_document.uri.as_str()).map(|d| analysis::symbols(&d.ws));
                    serde_json::to_value(ss.map(DocumentSymbolResponse::Nested))?
                } else if method == Completion::METHOD {
                    let p: CompletionParams = serde_json::from_value(params)?;
                    let tp = p.text_document_position;
                    let items =
                        docs.get_mut(tp.text_document.uri.as_str()).map(|d| analysis::completion(&mut d.ws, tp.position));
                    serde_json::to_value(items.map(CompletionResponse::Array))?
                } else if method == Formatting::METHOD {
                    let p: DocumentFormattingParams = serde_json::from_value(params)?;
                    let Some(d) = docs.get(p.text_document.uri.as_str()) else {
                        conn.sender.send(Message::Response(Response::new_ok(id, serde_json::Value::Null)))?;
                        continue;
                    };
                    match analysis::format(&d.ws) {
                        Ok(edits) => serde_json::to_value(edits)?,
                        Err(e) => {
                            let r = Response::new_err(id, ErrorCode::InternalError as i32, e.to_string());
                            conn.sender.send(Message::Response(r))?;
                            continue;
                        }
                    }
                } else {
                    let r = Response::new_err(id, ErrorCode::MethodNotFound as i32, format!("unsupported: {method}"));
                    conn.sender.send(Message::Response(r))?;
                    continue;
                };
                conn.sender.send(Message::Response(Response::new_ok(id, result)))?;
            }
            Message::Notification(n) => {
                if n.method == DidOpenTextDocument::METHOD {
                    let p: lsp_types::DidOpenTextDocumentParams = serde_json::from_value(n.params)?;
                    let Some(path) = uri::to_path(&p.text_document.uri) else { continue };
                    let path = path.canonicalize().unwrap_or(path);
                    let ws = Workspace::open_with(&path, p.text_document.text);
                    let doc = Doc { path, ws };
                    publish(&conn, p.text_document.uri.clone(), &doc)?;
                    docs.insert(p.text_document.uri.as_str().to_string(), doc);
                } else if n.method == DidChangeTextDocument::METHOD {
                    let p: lsp_types::DidChangeTextDocumentParams = serde_json::from_value(n.params)?;
                    let (Some(doc), Some(change)) = (docs.get_mut(p.text_document.uri.as_str()), p.content_changes.into_iter().last())
                    else {
                        continue;
                    };
                    let root = doc.ws.root;
                    if !doc.ws.set_source(root, change.text.clone()) {
                        // Imports changed: loading files is outside salsa.
                        doc.ws = Workspace::open_with(&doc.path, change.text);
                    }
                    publish(&conn, p.text_document.uri, doc)?;
                } else if n.method == DidCloseTextDocument::METHOD {
                    let p: lsp_types::DidCloseTextDocumentParams = serde_json::from_value(n.params)?;
                    docs.remove(p.text_document.uri.as_str());
                    let params = PublishDiagnosticsParams { uri: p.text_document.uri, diagnostics: vec![], version: None };
                    conn.sender.send(Message::Notification(Notification::new(PublishDiagnostics::METHOD.into(), params)))?;
                }
            }
            Message::Response(_) => {}
        }
    }
    // The writer thread ends when every sender is gone.
    drop(conn);
    io.join()?;
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
    let params = PublishDiagnosticsParams { uri, diagnostics, version: None };
    conn.sender.send(Message::Notification(Notification::new(PublishDiagnostics::METHOD.into(), params)))?;
    Ok(())
}
