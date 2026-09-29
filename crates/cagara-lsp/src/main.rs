//! `cagara-lsp`: a language server over stdio. Full-text sync; each open
//! document is the root of its own workspace, updated with `set_source`
//! (reloaded when its imports change). Imported files are read from disk.

use cagara_hir::workspace::Workspace;
use cagara_lsp::{analysis, uri};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{GotoDefinition, HoverRequest, Request as _};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, HoverProviderCapability, Location, MarkupContent, MarkupKind, OneOf, PublishDiagnosticsParams,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;

type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct Doc {
    path: PathBuf,
    ws: Workspace,
}

fn main() -> Res<()> {
    let (conn, io) = Connection::stdio();
    let caps = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
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
