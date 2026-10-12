//! poler-edit :: редакторский серверный протокол (LSP-стиль, JSON lines).
//!
//! `poler-engine --edit-serve`:
//! * stdin  — одна JSON-команда на строку: {"id":N,"cmd":"open","path":...}
//! * stdout — ответы {"id":N,"ok":true,...} и события {"ev":"progress",...}
//!
//! Потоковая модель:
//! * главный поток читает stdin и раздаёт задания воркеру (owning все буферы);
//! * воркер исполняет и пишет ответы/прогресс в общий stdout (через writer);
//! * отмена ("cancel") ставит AtomicBool НАПРЯМУЮ из главного потока —
//!   бегущий поиск/индексация видят её без очереди.
//!
//! GUI-клиент: integrations/poler-edit-qt (Qt6, Kate-подобный интерфейс).
//!
//! Команды:
//!   open {path} -> {doc}
//!   close {doc}
//!   stats {doc} -> {bytes,pieces,lines,indexed_bytes,edited,undo,redo}
//!   viewport {doc,line,count} -> {lines:[{line,text,truncated}]}
//!   goto {doc,line} -> {byte}
//!   linecol {doc,byte} -> {line,col}            (col — в символах)
//!   insert_at {doc,line,col,text}               (col — в символах)
//!   delete {doc,start_line,start_col,end_line,end_col}
//!   search {doc,query,case_sensitive,limit} -> {hits:[{byte,line,col,len}],...}
//!   index {doc} -> {lines}                      (с событиями progress)
//!   save {doc} / save_as {doc,path}
//!   undo {doc} / redo {doc}
//!   cancel                                      (отмена бегущего поиска/индекса)
//!   quit

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use serde_json::{json, Value};

use super::buffer::PolerBuffer;

/// Один процесс — много документов (вкладки GUI).
struct Docs {
    map: HashMap<u64, PolerBuffer>,
    next: u64,
}

impl Docs {
    fn new() -> Self {
        Self { map: HashMap::new(), next: 1 }
    }
    fn create(&mut self, b: PolerBuffer) -> u64 {
        let id = self.next;
        self.next += 1;
        self.map.insert(id, b);
        id
    }
}

enum Job {
    Open { id: u64, path: PathBuf },
    Close { id: u64, doc: u64 },
    Stats { id: u64, doc: u64 },
    Viewport { id: u64, doc: u64, line: u64, count: usize },
    Goto { id: u64, doc: u64, line: u64 },
    LineCol { id: u64, doc: u64, byte: u64 },
    InsertAt { id: u64, doc: u64, line: u64, col: u64, text: String },
    DeleteSel { id: u64, doc: u64, sl: u64, sc: u64, el: u64, ec: u64 },
    Search { id: u64, doc: u64, query: String, case_sensitive: bool, limit: usize },
    Index { id: u64, doc: u64 },
    Save { id: u64, doc: u64 },
    SaveAs { id: u64, doc: u64, path: PathBuf },
    Undo { id: u64, doc: u64 },
    Redo { id: u64, doc: u64 },
    Quit,
}

/// Точка входа `--edit-serve`. Возвращает код выхода.
pub fn run_edit_server() -> i32 {
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let (tx_out, rx_out) = mpsc::channel::<String>();
    let (tx_job, rx_job) = mpsc::channel::<Job>();

    // Писатель: единственный владелец stdout, flush после каждой строки.
    let writer = thread::spawn(move || -> io::Result<()> {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        for line in rx_out {
            let _ = writeln!(lock, "{line}");
            let _ = lock.flush();
        }
        Ok(())
    });

    // Воркер: владеет всеми документами, исполняет тяжёлые операции.
    let cancel_w = Arc::clone(&cancel);
    let tx_out_w = tx_out.clone();
    let worker = thread::spawn(move || {
        let mut docs = Docs::new();
        while let Ok(job) = rx_job.recv() {
            let resp = handle(&mut docs, job, &cancel_w, &tx_out_w);
            if let Some(line) = resp {
                let _ = tx_out_w.send(line);
            } else {
                break; // Quit
            }
        }
    });

    // Главный поток: stdin -> задания; cancel обслуживается мгновенно.
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut alive = true;
    while alive {
        let Some(Ok(line)) = lines.next() else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx_out.send(
                    json!({"id": 0, "ok": false, "error": format!("bad json: {e}")}).to_string(),
                );
                continue;
            }
        };
        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        let cmd = v.get("cmd").and_then(|x| x.as_str()).unwrap_or("");
        let job = match cmd {
            "open" => match req_path(&v) {
                Some(p) => Job::Open { id, path: p },
                None => {
                    err(&tx_out, id, "open: поле path обязательно");
                    continue;
                }
            },
            "close" => Job::Close { id, doc: doc_of(&v) },
            "stats" => Job::Stats { id, doc: doc_of(&v) },
            "viewport" => Job::Viewport {
                id,
                doc: doc_of(&v),
                line: u64_of(&v, "line"),
                count: u64_of(&v, "count").clamp(1, 4096) as usize,
            },
            "goto" => Job::Goto { id, doc: doc_of(&v), line: u64_of(&v, "line") },
            "linecol" => Job::LineCol { id, doc: doc_of(&v), byte: u64_of(&v, "byte") },
            "insert_at" => match v.get("text").and_then(|x| x.as_str()) {
                Some(t) => Job::InsertAt {
                    id,
                    doc: doc_of(&v),
                    line: u64_of(&v, "line"),
                    col: u64_of(&v, "col"),
                    // Защита от гигантских вставок через протокол.
                    text: t.chars().take(4 << 20).collect(),
                },
                None => {
                    err(&tx_out, id, "insert_at: поле text обязательно");
                    continue;
                }
            },
            "delete" => Job::DeleteSel {
                id,
                doc: doc_of(&v),
                sl: u64_of(&v, "start_line"),
                sc: u64_of(&v, "start_col"),
                el: u64_of(&v, "end_line"),
                ec: u64_of(&v, "end_col"),
            },
            "search" => Job::Search {
                id,
                doc: doc_of(&v),
                query: v
                    .get("query")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(4096)
                    .collect(),
                case_sensitive: v.get("case_sensitive").and_then(|x| x.as_bool()).unwrap_or(true),
                limit: u64_of(&v, "limit").clamp(1, 100_000) as usize,
            },
            "index" => Job::Index { id, doc: doc_of(&v) },
            "save" => Job::Save { id, doc: doc_of(&v) },
            "save_as" => match req_path(&v) {
                Some(p) => Job::SaveAs { id, doc: doc_of(&v), path: p },
                None => {
                    err(&tx_out, id, "save_as: поле path обязательно");
                    continue;
                }
            },
            "undo" => Job::Undo { id, doc: doc_of(&v) },
            "redo" => Job::Redo { id, doc: doc_of(&v) },
            "cancel" => {
                cancel.store(true, Ordering::Relaxed);
                let _ = tx_out.send(json!({"id": id, "ok": true, "cancelled": true}).to_string());
                continue;
            }
            "quit" => Job::Quit,
            other => {
                err(&tx_out, id, &format!("неизвестная команда: {other}"));
                continue;
            }
        };
        if matches!(job, Job::Quit) {
            alive = false;
        }
        if tx_job.send(job).is_err() {
            break;
        }
    }
    let _ = tx_job.send(Job::Quit);
    let _ = worker.join();
    drop(tx_out);
    let _ = writer.join();
    0
}

fn err(tx: &mpsc::Sender<String>, id: u64, msg: &str) {
    let _ = tx.send(json!({"id": id, "ok": false, "error": msg}).to_string());
}

fn doc_of(v: &Value) -> u64 {
    v.get("doc").and_then(|x| x.as_u64()).unwrap_or(0)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn req_path(v: &Value) -> Option<PathBuf> {
    v.get("path").and_then(|x| x.as_str()).map(PathBuf::from)
}

/// Байтовое смещение (line, col_символов) — col в СИМВОЛАХ строки.
fn byte_of_line_col(b: &mut PolerBuffer, line: u64, col: u64) -> io::Result<u64> {
    let start = b.offset_of_line(line)?;
    let one = b.viewport(line, 1)?;
    let text = one.first().map(|l| l.text.as_str()).unwrap_or("");
    let mut byte_col = text.len() as u64; // col за концом строки → в конец строки
    for (ci, (bi, _ch)) in text.char_indices().enumerate() {
        if ci as u64 == col {
            byte_col = bi as u64;
            break;
        }
    }
    Ok(start + byte_col)
}

/// (line, col_символов) для байтового смещения.
fn line_col_of_byte(b: &mut PolerBuffer, byte: u64) -> io::Result<(u64, u64)> {
    let line = b.line_of_offset(byte)?;
    let start = b.offset_of_line(line)?;
    let one = b.viewport(line, 1)?;
    let text = one.first().map(|l| l.text.as_str()).unwrap_or("");
    let prefix_len = byte.saturating_sub(start) as usize;
    let col = text
        .char_indices()
        .take_while(|(i, _)| *i < prefix_len)
        .count() as u64;
    Ok((line, col))
}

fn handle(
    docs: &mut Docs,
    job: Job,
    cancel: &Arc<AtomicBool>,
    tx: &mpsc::Sender<String>,
) -> Option<String> {
    let id = job_id(&job);
    let doc = job_doc(&job);
    // Каждая новая операция сбрасывает флаг отмены предыдущей.
    if !matches!(job, Job::Quit) {
        cancel.store(false, Ordering::Relaxed);
    }
    let r = match job {
        Job::Quit => return None,
        Job::Open { path, .. } => match PolerBuffer::open(&path) {
            Ok(b) => {
                let d = docs.create(b);
                let b = &docs.map[&d];
                json!({
                    "id": id, "ok": true, "doc": d,
                    "path": path,
                    "bytes": b.total_len(),
                    "pieces": b.pieces_len(),
                    "lines": b.lines_if_known(),
                })
            }
            Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
        },
        Job::Close { .. } => {
            docs.map.remove(&doc);
            json!({"id": id, "ok": true})
        }
        Job::Stats { .. } => match docs.map.get(&doc) {
            Some(b) => {
                let s = b.stats();
                json!({
                    "id": id, "ok": true,
                    "path": s.path,
                    "bytes": s.bytes,
                    "pieces": s.pieces,
                    "lines": s.lines,
                    "indexed_bytes": s.indexed_bytes,
                    "edited": s.edited,
                    "undo_depth": s.undo_depth,
                    "redo_depth": s.redo_depth,
                })
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Viewport { line, count, .. } => match docs.map.get_mut(&doc) {
            Some(b) => match b.viewport(line, count) {
                Ok(vp) => json!({
                    "id": id, "ok": true,
                    "lines": vp.iter().map(|l| json!({
                        "line": l.line, "text": l.text, "truncated": l.truncated,
                    })).collect::<Vec<_>>(),
                }),
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            },
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Goto { line, .. } => match docs.map.get_mut(&doc) {
            Some(b) => match b.offset_of_line(line) {
                Ok(byte) => json!({"id": id, "ok": true, "byte": byte}),
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            },
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::LineCol { byte, .. } => match docs.map.get_mut(&doc) {
            Some(b) => match line_col_of_byte(b, byte) {
                Ok((line, col)) => json!({"id": id, "ok": true, "line": line, "col": col}),
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            },
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::InsertAt { line, col, text, .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let r = byte_of_line_col(b, line, col)
                    .and_then(|off| b.insert(off, &text));
                match r {
                    Ok(()) => json!({"id": id, "ok": true, "bytes": b.total_len()}),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::DeleteSel { sl, sc, el, ec, .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let r = byte_of_line_col(b, sl, sc).and_then(|s| {
                    byte_of_line_col(b, el, ec).and_then(|e| b.delete(s, e))
                });
                match r {
                    Ok(()) => json!({"id": id, "ok": true, "bytes": b.total_len()}),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Search { query, case_sensitive, limit, .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let st = b.search(&query, case_sensitive, limit, cancel);
                match st {
                    Ok(st) => json!({
                        "id": id, "ok": true,
                        "hits": st.hits.iter().map(|h| json!({
                            "byte": h.byte, "line": h.line,
                            "col": h.col, "len": h.len,
                        })).collect::<Vec<_>>(),
                        "truncated": st.truncated,
                        "cancelled": st.cancelled,
                        "scanned_bytes": st.scanned_bytes,
                        "elapsed_ms": st.elapsed.as_millis() as u64,
                    }),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Index { .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let tx2 = tx.clone();
                let doc2 = doc;
                let started = std::time::Instant::now();
                let mut last = 0u128;
                let r = b.index_all(cancel, move |done, total| {
                    let now = started.elapsed().as_millis();
                    if now.saturating_sub(last) >= 120 || done == total {
                        last = now;
                        let gbps = if now > 0 {
                            (done as f64 / 1024.0 / 1024.0 / 1024.0) / (now as f64 / 1000.0)
                        } else {
                            0.0
                        };
                        let _ = tx2.send(
                            json!({
                                "ev": "progress", "op": "index", "doc": doc2,
                                "done_bytes": done, "total_bytes": total,
                                "gbps": (gbps * 10.0).round() / 10.0,
                            })
                            .to_string(),
                        );
                    }
                });
                match r {
                    Ok((lines, done)) => json!({
                        "id": id, "ok": true, "lines": lines, "completed": done,
                    }),
                    Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
                }
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Save { .. } => match docs.map.get_mut(&doc) {
            Some(b) => match b.save() {
                Ok(n) => json!({"id": id, "ok": true, "bytes": n, "path": b.path()}),
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            },
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::SaveAs { path, .. } => match docs.map.get_mut(&doc) {
            Some(b) => match b.save_as(&path) {
                Ok(n) => json!({"id": id, "ok": true, "bytes": n, "path": path}),
                Err(e) => json!({"id": id, "ok": false, "error": e.to_string()}),
            },
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Undo { .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let applied = b.undo();
                json!({"id": id, "ok": true, "applied": applied})
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
        Job::Redo { .. } => match docs.map.get_mut(&doc) {
            Some(b) => {
                let applied = b.redo();
                json!({"id": id, "ok": true, "applied": applied})
            }
            None => json!({"id": id, "ok": false, "error": "нет такого doc"}),
        },
    };
    Some(r.to_string())
}

fn job_id(j: &Job) -> u64 {
    match j {
        Job::Quit => 0,
        Job::Open { id, .. }
        | Job::Close { id, .. }
        | Job::Stats { id, .. }
        | Job::Viewport { id, .. }
        | Job::Goto { id, .. }
        | Job::LineCol { id, .. }
        | Job::InsertAt { id, .. }
        | Job::DeleteSel { id, .. }
        | Job::Search { id, .. }
        | Job::Index { id, .. }
        | Job::Save { id, .. }
        | Job::SaveAs { id, .. }
        | Job::Undo { id, .. }
        | Job::Redo { id, .. } => *id,
    }
}

fn job_doc(j: &Job) -> u64 {
    match j {
        Job::Open { .. } | Job::Quit => 0,
        Job::Close { doc, .. }
        | Job::Stats { doc, .. }
        | Job::Viewport { doc, .. }
        | Job::Goto { doc, .. }
        | Job::LineCol { doc, .. }
        | Job::InsertAt { doc, .. }
        | Job::DeleteSel { doc, .. }
        | Job::Search { doc, .. }
        | Job::Index { doc, .. }
        | Job::Save { doc, .. }
        | Job::SaveAs { doc, .. }
        | Job::Undo { doc, .. }
        | Job::Redo { doc, .. } => *doc,
    }
}
