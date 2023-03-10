use std::sync::Arc;
use tokio::select;
use tokio::sync::mpsc;

use crate::semantic::*;
use crate::util::*;
use hashbrown::HashMap;
use log::info;
use ropey::Rope;
use tokio::time::Instant;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tree_sitter::{Node, QueryCursor, Tree};

/*
 * Most error checking happens in here.
 * Files are checked in three phases if one phase failes checking is stopped.
 * Phase1. Check Sanity: Here we check if the file is ambiguous,
 * this happens for example if there is a missing opperand at line break
 * Phase2. Check References: When all files have correct syntax we check if pathes are valid and
 * have the correct type
 *
 *
 * All erros have a artificial severity weight to mask consequential errors.
*/

#[derive(Clone, Debug)]
pub struct ErrorInfo {
    pub location: Range,
    pub severity: DiagnosticSeverity,
    pub weight: u32,
    pub msg: String,
}

impl ErrorInfo {
    fn diagnostic(self) -> Diagnostic {
        Diagnostic {
            range: self.location,
            severity: Some(self.severity),
            message: self.msg,
            ..Default::default()
        }
    }
}
pub async fn publish(client: &Client, uri: &Url, err: &[ErrorInfo]) {
    if let Some(max) = err.iter().max_by_key(|e| e.weight) {
        client
            .publish_diagnostics(
                uri.clone(),
                err.iter()
                    .rev()
                    .filter(|e| e.weight == max.weight)
                    .map(|i| i.clone().diagnostic())
                    .collect(),
                None,
            )
            .await;
    } else {
        client.publish_diagnostics(uri.clone(), vec![], None).await;
    }
}
//Walk the syntax tree and only go "down" if F is true
fn ts_filterd_visit<F: FnMut(Node) -> bool>(root: Node, mut f: F) {
    let mut reached_root = false;
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return;
    }
    while !reached_root {
        if f(cursor.node()) && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.node() == root {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}
//Check if line breaks are correct eg. inside parenthesis
//This is necessary because the treesitter grammer allows 2 features on the same line under certain
//conditions.
pub fn check_sanity(tree: &Tree, source: &Rope) -> Vec<ErrorInfo> {
    let time = Instant::now();
    let mut cursor = QueryCursor::new();
    let mut error = Vec::new();
    let mut lines = HashMap::new();
    for i in cursor.matches(
        &TS.queries.check_sanity,
        tree.root_node(),
        node_source(source),
    ) {
        let node = i.captures[0].node;
        //We have to check if line breaks in expression are contained by a parenthesis
        if node.kind() == "expr" {
            if node.start_position().row == node.end_position().row {
                continue;
            } else {
                let mut ok_lines = Vec::new();
                ts_filterd_visit(node, |node| {
                    if node.kind() == "nested_expr" {
                        for i in node.start_position().row..node.end_position().row {
                            ok_lines.push(i);
                        }
                        false
                    } else {
                        true
                    }
                });
                for i in node.start_position().row..node.end_position().row {
                    if !ok_lines.iter().any(|k| *k == i) {
                        error.push(ErrorInfo {
                            weight: 100,
                            location: node_range(node, source),
                            severity: DiagnosticSeverity::ERROR,
                            msg: "line breaks are only allowed inside parenthesis".to_string(),
                        });
                    }
                }
            }
        } else if i.pattern_index == 0 {
            //Header
            if node.start_position().row != node.end_position().row {
                error.push(ErrorInfo {
                    weight: 100,
                    location: node_range(node, source),
                    severity: DiagnosticSeverity::ERROR,
                    msg: "line breaks are only allowed inside parenthesis".to_string(),
                });
            }
            if lines.insert(node.start_position().row, node).is_some() {
                error.push(ErrorInfo {
                    weight: 100,
                    location: node_range(node, source),
                    severity: DiagnosticSeverity::ERROR,
                    msg: "features have to be in diffrent lines".to_string(),
                });
            }
        } else {
            //check name or string since quoted names allow line breaks in ts but should not
            if node.start_position().row != node.end_position().row {
                error.push(ErrorInfo {
                    weight: 100,
                    location: node_range(node, source),
                    severity: DiagnosticSeverity::ERROR,
                    msg: "multiline strings are not supported".to_string(),
                });
            }
        }
    }
    info!(" check sanity in {:?}", time.elapsed());
    error
}

pub fn classify_error(root: Node, source: &Rope) -> ErrorInfo {
    let err_source = source.byte_slice(root.byte_range());
    if root.start_position().row == root.end_position().row {
        let err_raw: String = err_source.into();
        if err_raw.contains("=>")
            || err_raw.contains("<=>")
            || err_raw.contains('&')
            || err_raw.contains('|')
            || err_raw.contains('+')
            || err_raw.contains('-')
            || err_raw.contains('*')
            || err_raw.contains('/')
            || err_raw.contains('>')
            || err_raw.contains('<')
            || err_raw.contains("==")
        {
            return ErrorInfo {
                location: node_range(root, source),
                severity: DiagnosticSeverity::ERROR,
                weight: 80,
                msg: "missing lhs or rhs expression".into(),
            };
        }
    }
    ErrorInfo {
        location: node_range(root, source),
        severity: DiagnosticSeverity::ERROR,
        weight: 80,
        msg: "unknown syntax error".into(),
    }
}
pub fn check_errors(tree: &Tree, source: &Rope) -> Vec<ErrorInfo> {
    let mut err: Vec<ErrorInfo> = Vec::new();
    ts_filterd_visit(tree.root_node(), |i| {
        if i.is_missing() {
            err.push(ErrorInfo {
                location: node_range(i, source),
                severity: DiagnosticSeverity::ERROR,
                weight: 80,
                msg: format!("missing {}", i.kind()),
            });
            false
        } else if i.is_error() {
            err.push(classify_error(i, source));
            false
        } else {
            true
        }
    });
    err
}

pub struct DiagnosticUpdate {
    pub error_state: HashMap<Url, Vec<ErrorInfo>>,
    pub timestamp: u64,
}
struct DiagnosticState {
    timestamp: u64,
    error: Vec<ErrorInfo>,
}
async fn maybe_publish(
    client: &Client,
    source_map: &mut HashMap<Url, DiagnosticState>,
    uri: Url,
    mut err: Vec<ErrorInfo>,
    timestamp: u64,
) {
    if let Some(old) = source_map.get_mut(&uri) {

        if old.timestamp < timestamp {
            publish(client, &uri, &err).await;
            old.timestamp = timestamp;
            old.error = err;
        } else if old.timestamp == timestamp {
            old.timestamp = timestamp;
            old.error.append(&mut err);
            publish(client, &uri, &old.error).await;
        }
    } else {
        publish(client, &uri, &err).await;
        source_map.insert(
            uri,
            DiagnosticState {
                timestamp,
                error: err,
            },
        );
    }
}

pub async fn diagnostic_handler(ctx: Arc<Context>, mut rx: mpsc::Receiver<DiagnosticUpdate>) {
    let mut source_map: HashMap<Url, DiagnosticState> = HashMap::new();
    loop {
        select! {
            _ = ctx.shutdown.cancelled() => return,
            Some(mut update) = rx.recv()=>{
                for (uri,err) in update.error_state.drain(){
                    maybe_publish(&ctx.client,&mut source_map,uri,err,update.timestamp).await

                }

            }

        }
    }
}
