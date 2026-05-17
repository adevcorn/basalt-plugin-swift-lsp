#![allow(clippy::missing_safety_doc)]
#![allow(static_mut_refs)]

use std::collections::HashMap;
use std::mem;

const BASALT_PLUGIN_API_VERSION: u32 = 1;
const CAP_DIAGNOSTICS: u64 = 1 << 0;
const CAP_HOVER: u64 = 1 << 9;
const CAP_SEMANTIC_TOKENS: u64 = 1 << 11;
const CAP_SYMBOL_RELATIONS: u64 = 1 << 12;
/// Tells the host to call `build_project_model` with a per-file-inferred
/// project root before each semantic dispatch.  Must match
/// `CapabilityFlags::SEMANTIC_NEEDS_PROJECT_MODEL` in core/src/plugin_meta.rs.
const CAP_SEMANTIC_NEEDS_PROJECT_MODEL: u64 = 1 << 15;

const LOG_INFO: i32 = 1;
const LOG_ERROR: i32 = 3;

const READ_BUF_OFFSET: usize = 10 * 1024 * 1024;
const READ_BUF_CAP: usize = 256 * 1024;
const WAIT_INIT_MS: usize = 10_000;
const WAIT_HOVER_MS: usize = 5_000;
const WAIT_DIAGNOSTIC_MS: usize = 750;
const WAIT_SEMANTIC_MS: usize = 10_000;
const POLL_SLEEP_MS: i32 = 2;

#[repr(u8)]
#[derive(Clone, Copy)]
enum Severity {
    Error = 0,
    Warning = 1,
    Info = 2,
    Hint = 3,
}

#[derive(Clone)]
struct DiagnosticRecord {
    offset: u32,
    length: u32,
    message: String,
    severity: Severity,
}

#[derive(Clone)]
struct DocumentState {
    version: i32,
    text: String,
}

#[derive(Clone)]
struct SemanticLegend {
    token_types: Vec<String>,
    token_modifiers: Vec<String>,
}

struct Session {
    root: String,
    handle: i32,
    initialized: bool,
    next_id: u32,
    read_buf: Vec<u8>,
    docs: HashMap<String, DocumentState>,
    diagnostics: HashMap<String, Vec<DiagnosticRecord>>,
    semantic_legend: Option<SemanticLegend>,
}

impl Session {
    fn new(root: String, handle: i32) -> Self {
        Self {
            root,
            handle,
            initialized: false,
            next_id: 1,
            read_buf: Vec::new(),
            docs: HashMap::new(),
            diagnostics: HashMap::new(),
            semantic_legend: None,
        }
    }
}

#[derive(Default)]
struct PluginState {
    session: Option<Session>,
}

static mut STATE: Option<PluginState> = None;

fn state() -> &'static mut PluginState {
    unsafe {
        if STATE.is_none() {
            STATE = Some(PluginState::default());
        }
        STATE.as_mut().unwrap()
    }
}

#[link(wasm_import_module = "env")]
extern "C" {
    fn basalt_log(level: i32, msg_ptr: i32, msg_len: i32);
    fn basalt_spawn_lsp(cmd_ptr: i32, cmd_len: i32, root_ptr: i32, root_len: i32) -> i32;
    fn basalt_lsp_write(handle: i32, buf_ptr: i32, buf_len: i32) -> i32;
    fn basalt_lsp_read(handle: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_lsp_stop(handle: i32);
    fn basalt_sleep_ms(ms: i32);
}

fn log(level: i32, message: &str) {
    unsafe {
        basalt_log(level, message.as_ptr() as i32, message.len() as i32);
    }
}

fn lsp_command_bytes() -> &'static [u8] {
    b"/usr/bin/xcrun\0sourcekit-lsp"
}

fn spawn_session(root: &str) -> Option<Session> {
    let cmd = lsp_command_bytes();
    let handle = unsafe {
        basalt_spawn_lsp(
            cmd.as_ptr() as i32,
            cmd.len() as i32,
            root.as_ptr() as i32,
            root.len() as i32,
        )
    };
    if handle < 0 {
        log(
            LOG_ERROR,
            "failed to spawn sourcekit-lsp via /usr/bin/xcrun",
        );
        None
    } else {
        Some(Session::new(root.to_string(), handle))
    }
}

fn ensure_session(root: &str) -> Option<&'static mut Session> {
    let state = state();
    let needs_replace = match state.session.as_ref() {
        Some(session) => session.root != root,
        None => true,
    };
    if needs_replace {
        if let Some(old) = state.session.take() {
            unsafe {
                basalt_lsp_stop(old.handle);
            }
        }
        state.session = spawn_session(root);
    }
    state.session.as_mut()
}

fn ensure_initialized(session: &mut Session) -> bool {
    if session.initialized {
        return true;
    }

    let root_uri = path_to_file_uri(&session.root);
    let name = path_basename(&session.root);
    let request_id = session.next_id;
    session.next_id += 1;

    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"initialize\",\"params\":{{\"processId\":null,\"rootPath\":\"{}\",\"rootUri\":\"{}\",\"clientInfo\":{{\"name\":\"Basalt Swift LSP\",\"version\":\"0.1.0\"}},\"capabilities\":{{\"textDocument\":{{\"publishDiagnostics\":{{\"relatedInformation\":true}},\"semanticTokens\":{{\"dynamicRegistration\":false,\"requests\":{{\"full\":true}},\"formats\":[\"relative\"],\"tokenTypes\":[\"namespace\",\"type\",\"class\",\"enum\",\"protocol\",\"struct\",\"typeParameter\",\"parameter\",\"variable\",\"property\",\"enumMember\",\"function\",\"method\",\"macro\",\"keyword\",\"modifier\",\"comment\",\"string\",\"number\",\"operator\"],\"tokenModifiers\":[\"declaration\",\"definition\",\"readonly\",\"static\",\"defaultLibrary\",\"deprecated\",\"async\"]}}}}}},\"workspaceFolders\":[{{\"uri\":\"{}\",\"name\":\"{}\"}}]}}}}",
        escape_json(&session.root),
        escape_json(&root_uri),
        escape_json(&root_uri),
        escape_json(&name),
    );

    if !send_json(session, &json) {
        return false;
    }

    let Some(response_text) = wait_for_response_text(session, request_id, WAIT_INIT_MS) else {
        log(LOG_ERROR, "sourcekit-lsp initialize timed out");
        return false;
    };
    if initialize_response_has_error(&response_text) {
        log(LOG_ERROR, "sourcekit-lsp initialize returned an error");
        return false;
    }

    session.semantic_legend = extract_semantic_legend_from_raw(&response_text);
    if !send_json(
        session,
        "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}",
    ) {
        return false;
    }

    session.initialized = true;
    true
}

fn wait_for_response_text(session: &mut Session, id: u32, timeout_ms: usize) -> Option<String> {
    let polls = (timeout_ms / (POLL_SLEEP_MS as usize)).max(1);
    for _ in 0..polls {
        let _ = read_available(session);
        while let Some(message) = extract_message(&mut session.read_buf) {
            let seen_id = raw_response_id(&message);
            if seen_id == Some(id) {
                return Some(message);
            }
            if let Some(value) = parse_json(&message) {
                handle_message(session, &value);
            }
        }
        unsafe { basalt_sleep_ms(POLL_SLEEP_MS) };
    }
    if !session.read_buf.is_empty() {
        let preview_len = session.read_buf.len().min(240);
        let preview = String::from_utf8_lossy(&session.read_buf[..preview_len])
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        log(
            LOG_INFO,
            &format!(
                "timed out waiting for id {id} with {} buffered bytes: {preview}",
                session.read_buf.len()
            ),
        );
    }
    None
}

fn raw_response_id(message: &str) -> Option<u32> {
    let key = "\"id\":";
    let start = message.find(key)? + key.len();
    let rest = &message[start..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u32>().ok()
}

fn initialize_response_has_error(message: &str) -> bool {
    message.contains("\"error\":")
}

fn extract_semantic_legend_from_raw(message: &str) -> Option<SemanticLegend> {
    Some(SemanticLegend {
        token_types: extract_string_array_field(message, "tokenTypes")?,
        token_modifiers: extract_string_array_field(message, "tokenModifiers").unwrap_or_default(),
    })
}

fn extract_string_array_field(message: &str, field: &str) -> Option<Vec<String>> {
    let key = format!("\"{}\":[", field);
    let start = message.find(&key)? + key.len();
    let rest = &message[start..];
    let end = rest.find(']')?;
    let body = &rest[..end];
    let mut out = Vec::new();
    for part in body.split(',') {
        let trimmed = part.trim();
        let unquoted = trimmed.strip_prefix('"')?.strip_suffix('"')?;
        out.push(unquoted.replace("\\/", "/").replace("\\\"", "\""));
    }
    Some(out)
}

fn send_json(session: &mut Session, json: &str) -> bool {
    let frame = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
    let written =
        unsafe { basalt_lsp_write(session.handle, frame.as_ptr() as i32, frame.len() as i32) };
    written == frame.len() as i32
}

fn read_available(session: &mut Session) -> bool {
    let mut saw_bytes = false;
    loop {
        let n =
            unsafe { basalt_lsp_read(session.handle, READ_BUF_OFFSET as i32, READ_BUF_CAP as i32) };
        if n < 0 {
            return saw_bytes;
        }
        if n == 0 {
            return saw_bytes;
        }
        let bytes = unsafe { std::slice::from_raw_parts(READ_BUF_OFFSET as *const u8, n as usize) };
        session.read_buf.extend_from_slice(bytes);
        saw_bytes = true;
    }
}

fn extract_message(buf: &mut Vec<u8>) -> Option<String> {
    let header_end = find_bytes(buf, b"\r\n\r\n")?;
    let header = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let content_length = content_length?;
    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    let body = String::from_utf8(buf[body_start..body_start + content_length].to_vec()).ok()?;
    buf.drain(..body_start + content_length);
    Some(body)
}

fn wait_for_response(session: &mut Session, id: u32, rounds: usize) -> Option<JsonValue> {
    let polls = (rounds / (POLL_SLEEP_MS as usize)).max(1);
    for _ in 0..polls {
        let _ = read_available(session);
        while let Some(message) = extract_message(&mut session.read_buf) {
            let Some(value) = parse_json(&message) else {
                let preview: String = message.chars().take(240).collect();
                log(
                    LOG_INFO,
                    &format!("failed to parse LSP message for id {id}: {preview}"),
                );
                continue;
            };
            if matches_response_id(&value, id) {
                return Some(value);
            }
            handle_message(session, &value);
        }
        unsafe { basalt_sleep_ms(POLL_SLEEP_MS) };
    }
    if !session.read_buf.is_empty() {
        let preview_len = session.read_buf.len().min(240);
        let preview = String::from_utf8_lossy(&session.read_buf[..preview_len])
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        log(
            LOG_INFO,
            &format!(
                "timed out waiting for id {id} with {} buffered bytes: {preview}",
                session.read_buf.len()
            ),
        );
    }
    None
}

fn drain_messages(session: &mut Session, timeout_ms: usize) {
    let polls = (timeout_ms / (POLL_SLEEP_MS as usize)).max(1);
    for _ in 0..polls {
        let read = read_available(session);
        let mut saw_message = false;
        while let Some(message) = extract_message(&mut session.read_buf) {
            saw_message = true;
            if let Some(value) = parse_json(&message) {
                handle_message(session, &value);
            }
        }
        if !read && !saw_message {
            break;
        }
        unsafe { basalt_sleep_ms(POLL_SLEEP_MS) };
    }
}

fn handle_message(session: &mut Session, value: &JsonValue) {
    let Some(root) = value.as_object() else {
        return;
    };
    let Some(JsonValue::String(method)) = root.get("method") else {
        return;
    };
    if method != "textDocument/publishDiagnostics" {
        return;
    }
    let Some(params) = root.get("params").and_then(JsonValue::as_object) else {
        return;
    };
    let Some(uri) = params.get("uri").and_then(JsonValue::as_str) else {
        return;
    };
    let path = file_uri_to_path(uri);
    let Some(doc) = session.docs.get(&path) else {
        return;
    };
    let Some(diags) = params.get("diagnostics").and_then(JsonValue::as_array) else {
        return;
    };

    let mut out = Vec::new();
    for diag in diags {
        let Some(obj) = diag.as_object() else {
            continue;
        };
        let Some(message) = obj.get("message").and_then(JsonValue::as_str) else {
            continue;
        };
        let severity = match obj.get("severity").and_then(JsonValue::as_u64) {
            Some(1) => Severity::Error,
            Some(2) => Severity::Warning,
            Some(3) => Severity::Info,
            Some(4) => Severity::Hint,
            _ => Severity::Warning,
        };
        let Some(range) = obj.get("range").and_then(JsonValue::as_object) else {
            continue;
        };
        let Some(start) = range.get("start").and_then(JsonValue::as_object) else {
            continue;
        };
        let Some(end) = range.get("end").and_then(JsonValue::as_object) else {
            continue;
        };
        let start_line = start.get("line").and_then(JsonValue::as_u64).unwrap_or(0) as usize;
        let start_char = start
            .get("character")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let end_line = end
            .get("line")
            .and_then(JsonValue::as_u64)
            .unwrap_or(start_line as u64) as usize;
        let end_char = end
            .get("character")
            .and_then(JsonValue::as_u64)
            .unwrap_or(start_char as u64) as usize;
        let start_offset = position_to_byte_offset(&doc.text, start_line, start_char);
        let end_offset = position_to_byte_offset(&doc.text, end_line, end_char);
        let length = end_offset.saturating_sub(start_offset).max(1);
        out.push(DiagnosticRecord {
            offset: start_offset as u32,
            length: length as u32,
            message: message.to_string(),
            severity,
        });
    }

    session.diagnostics.insert(path, out);
}

fn sync_document(session: &mut Session, path: &str, text: &str) -> bool {
    if !ensure_initialized(session) {
        return false;
    }

    let uri = path_to_file_uri(path);
    let next_version = session
        .docs
        .get(path)
        .map(|doc| doc.version + 1)
        .unwrap_or(1);
    let changed = session
        .docs
        .get(path)
        .map(|doc| doc.text != text)
        .unwrap_or(true);

    if !session.docs.contains_key(path) {
        let json = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\",\"languageId\":\"swift\",\"version\":{},\"text\":\"{}\"}}}}}}",
            escape_json(&uri),
            next_version,
            escape_json(text),
        );
        if !send_json(session, &json) {
            return false;
        }
    } else if changed {
        let json = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\",\"version\":{}}},\"contentChanges\":[{{\"text\":\"{}\"}}]}}}}",
            escape_json(&uri),
            next_version,
            escape_json(text),
        );
        if !send_json(session, &json) {
            return false;
        }
    }

    session.docs.insert(
        path.to_string(),
        DocumentState {
            version: next_version,
            text: text.to_string(),
        },
    );
    true
}

fn best_root(root: &str) -> String {
    if root.is_empty() {
        ".".to_string()
    } else {
        root.to_string()
    }
}

fn absolute_path(root: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if root.ends_with('/') {
        format!("{root}{path}")
    } else {
        format!("{root}/{path}")
    }
}

fn encode_diagnostics(diags: &[DiagnosticRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for diag in diags {
        let msg = diag.message.as_bytes();
        let msg_len = msg.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&diag.offset.to_le_bytes());
        out.extend_from_slice(&diag.length.to_le_bytes());
        out.extend_from_slice(&msg_len.to_le_bytes());
        out.push(diag.severity as u8);
        out.push(0);
        out.extend_from_slice(&msg[..msg_len as usize]);
    }
    out
}

fn pack_output(bytes: Vec<u8>) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_mut_ptr() as u64;
    let len = boxed.len() as u64;
    mem::forget(boxed);
    ((ptr << 32) | len) as i64
}

#[no_mangle]
pub unsafe extern "C" fn deallocate(ptr: i32, len: i32) {
    if ptr <= 0 || len <= 0 {
        return;
    }
    let _ = Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize);
}

#[repr(C)]
struct PluginMetaRecord {
    api_version: u32,
    _pad: u32,
    hook_flags: u64,
    name_ptr: u32,
    version_ptr: u32,
    provides_ptr: u32,
    requires_ptr: u32,
    file_globs_ptr: u32,
    activates_on_ptr: u32,
    activation_events_ptr: u32,
}

static NAME: &[u8] = b"swift-lsp\0";
static VERSION: &[u8] = b"0.1.0\0";
static PROVIDES: &[u8] =
    b"diagnostics:swift\nhover:swift\nsemantic-tokens:swift\nsymbol-relations:swift\0";
static REQUIRES: &[u8] = b"\0";
static FILE_GLOBS: &[u8] = b"*.swift\n**/*.swift\nPackage.swift\0";
static ACTIVATES_ON: &[u8] = b"Package.swift\n*.xcodeproj/project.pbxproj\0";
static ACTIVATION_EVENTS: &[u8] = b"workspace_opened\0";
static mut META: PluginMetaRecord = PluginMetaRecord {
    api_version: 0,
    _pad: 0,
    hook_flags: 0,
    name_ptr: 0,
    version_ptr: 0,
    provides_ptr: 0,
    requires_ptr: 0,
    file_globs_ptr: 0,
    activates_on_ptr: 0,
    activation_events_ptr: 0,
};

#[no_mangle]
pub extern "C" fn basalt_plugin_metadata() -> i32 {
    unsafe {
        META.api_version = BASALT_PLUGIN_API_VERSION;
        META._pad = 0;
        META.hook_flags = CAP_DIAGNOSTICS | CAP_HOVER | CAP_SEMANTIC_TOKENS | CAP_SYMBOL_RELATIONS | CAP_SEMANTIC_NEEDS_PROJECT_MODEL;
        META.name_ptr = NAME.as_ptr() as u32;
        META.version_ptr = VERSION.as_ptr() as u32;
        META.provides_ptr = PROVIDES.as_ptr() as u32;
        META.requires_ptr = REQUIRES.as_ptr() as u32;
        META.file_globs_ptr = FILE_GLOBS.as_ptr() as u32;
        META.activates_on_ptr = ACTIVATES_ON.as_ptr() as u32;
        META.activation_events_ptr = ACTIVATION_EVENTS.as_ptr() as u32;
        &raw const META as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn basalt_build_project_model(root_ptr: i32, root_len: i32) -> i64 {
    let root = read_utf8(root_ptr, root_len);
    if root.is_empty() {
        return 0;
    }
    let root = best_root(&root);
    let Some(session) = ensure_session(&root) else {
        return 0;
    };
    let _ = ensure_initialized(session);
    0
}

#[no_mangle]
pub unsafe extern "C" fn basalt_diagnose(
    src_ptr: i32,
    src_len: i32,
    path_ptr: i32,
    path_len: i32,
) -> i64 {
    let src = read_utf8(src_ptr, src_len);
    let rel_path = read_utf8(path_ptr, path_len);
    let Some(session) = state().session.as_mut() else {
        return 0;
    };
    let full_path = absolute_path(&session.root, &rel_path);
    if !sync_document(session, &full_path, &src) {
        return 0;
    }
    drain_messages(session, WAIT_DIAGNOSTIC_MS);
    let diags = session
        .diagnostics
        .get(&full_path)
        .cloned()
        .unwrap_or_default();
    pack_output(encode_diagnostics(&diags))
}

#[no_mangle]
pub unsafe extern "C" fn basalt_hover(
    src_ptr: i32,
    src_len: i32,
    path_ptr: i32,
    path_len: i32,
    byte_offset: i32,
) -> i64 {
    let src = read_utf8(src_ptr, src_len);
    let rel_path = read_utf8(path_ptr, path_len);
    let Some(session) = state().session.as_mut() else {
        return 0;
    };
    let full_path = absolute_path(&session.root, &rel_path);
    if !sync_document(session, &full_path, &src) {
        return 0;
    }

    let (line, character) = byte_offset_to_position(&src, byte_offset.max(0) as usize);
    let request_id = session.next_id;
    session.next_id += 1;
    let uri = path_to_file_uri(&full_path);
    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"textDocument/hover\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\"}},\"position\":{{\"line\":{},\"character\":{}}}}}}}",
        escape_json(&uri),
        line,
        character,
    );
    if !send_json(session, &json) {
        return 0;
    }

    let Some(response) = wait_for_response(session, request_id, WAIT_HOVER_MS) else {
        return 0;
    };
    let markdown = extract_hover_markdown(&response).unwrap_or_default();
    pack_output(markdown.into_bytes())
}

#[no_mangle]
pub unsafe extern "C" fn basalt_semantic_tokens(
    src_ptr: i32,
    src_len: i32,
    path_ptr: i32,
    path_len: i32,
) -> i64 {
    let src = read_utf8(src_ptr, src_len);
    let rel_path = read_utf8(path_ptr, path_len);
    let Some(session) = state().session.as_mut() else {
        log(
            LOG_INFO,
            &format!("semantic tokens unavailable for {rel_path}: no active sourcekit-lsp session"),
        );
        return 0;
    };
    let full_path = absolute_path(&session.root, &rel_path);
    if !sync_document(session, &full_path, &src) {
        log(
            LOG_INFO,
            &format!("semantic tokens skipped for {full_path}: document sync failed"),
        );
        return 0;
    }
    let Some(legend) = session.semantic_legend.clone() else {
        log(
            LOG_INFO,
            &format!("semantic tokens unavailable for {full_path}: no legend from sourcekit-lsp"),
        );
        return 0;
    };

    let request_id = session.next_id;
    session.next_id += 1;
    let uri = path_to_file_uri(&full_path);
    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"textDocument/semanticTokens/full\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\"}}}}}}",
        escape_json(&uri),
    );
    if !send_json(session, &json) {
        log(
            LOG_INFO,
            &format!("semantic tokens request write failed for {full_path}"),
        );
        return 0;
    }

    let Some(response_text) = wait_for_response_text(session, request_id, WAIT_SEMANTIC_MS) else {
        log(
            LOG_INFO,
            &format!("semantic tokens request timed out for {full_path}"),
        );
        return 0;
    };
    let Some(data) = extract_semantic_data_from_raw(&response_text) else {
        log(
            LOG_INFO,
            &format!("semantic tokens response empty for {full_path}"),
        );
        return 0;
    };
    let encoded = encode_semantic_tokens(&src, &legend, &data);
    pack_output(encoded)
}

#[no_mangle]
pub unsafe extern "C" fn basalt_symbol_relations(
    src_ptr: i32,
    src_len: i32,
    path_ptr: i32,
    path_len: i32,
    byte_offset: i32,
) -> i64 {
    let src = read_utf8(src_ptr, src_len);
    let rel_path = read_utf8(path_ptr, path_len);
    let Some(session) = state().session.as_mut() else {
        return 0;
    };
    let full_path = absolute_path(&session.root, &rel_path);
    if !sync_document(session, &full_path, &src) {
        return 0;
    }

    let (line, character) = byte_offset_to_position(&src, byte_offset.max(0) as usize);
    let uri = path_to_file_uri(&full_path);

    let declarations = request_location_method(
        session,
        "textDocument/declaration",
        &uri,
        line,
        character,
        WAIT_HOVER_MS,
    )
    .or_else(|| {
        request_location_method(
            session,
            "textDocument/definition",
            &uri,
            line,
            character,
            WAIT_HOVER_MS,
        )
    });

    let references = match request_document_highlights(session, &uri, line, character, WAIT_HOVER_MS) {
        Some(highlights) if !highlights.is_empty() => highlights,
        _ => request_references(session, &uri, line, character, WAIT_HOVER_MS).unwrap_or_default(),
    };
    let implementations = request_location_method(
        session,
        "textDocument/implementation",
        &uri,
        line,
        character,
        WAIT_HOVER_MS,
    )
    .unwrap_or_default();

    let mut out = Vec::new();
    for loc in declarations.unwrap_or_default() {
        if let Some((offset, length)) = location_to_span_for_uri(&src, &full_path, &loc) {
            encode_symbol_relation_record(&mut out, offset, length, 1);
        }
    }
    for loc in references {
        if let Some((offset, length)) = document_highlight_to_span(&src, &loc)
            .or_else(|| location_to_span_for_uri(&src, &full_path, &loc)) {
            encode_symbol_relation_record(&mut out, offset, length, 2);
        }
    }
    for loc in implementations {
        if let Some((offset, length)) = location_to_span_for_uri(&src, &full_path, &loc) {
            encode_symbol_relation_record(&mut out, offset, length, 3);
        }
    }
    pack_output(out)
}

unsafe fn read_utf8(ptr: i32, len: i32) -> String {
    if ptr <= 0 || len <= 0 {
        return String::new();
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize);
    String::from_utf8_lossy(bytes).into_owned()
}

fn path_to_file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    if !path.starts_with('/') {
        out.push('/');
    }
    for &b in path.as_bytes() {
        if is_uri_unreserved(b) || b == b'/' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex((b >> 4) & 0x0f));
            out.push(hex(b & 0x0f));
        }
    }
    out
}

fn file_uri_to_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(raw)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (from_hex(bytes[index + 1]), from_hex(bytes[index + 2])) {
                out.push((a << 4) | b);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn path_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn is_uri_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

fn hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let n = c as u32;
                out.push_str("\\u");
                out.push(hex(((n >> 12) & 0xf) as u8));
                out.push(hex(((n >> 8) & 0xf) as u8));
                out.push(hex(((n >> 4) & 0xf) as u8));
                out.push(hex((n & 0xf) as u8));
            }
            c => out.push(c),
        }
    }
    out
}

fn byte_offset_to_position(text: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = byte_offset.min(text.len());
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (idx, ch) in text.char_indices() {
        if idx >= clamped {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    let slice = &text[line_start..clamped];
    let character = slice.encode_utf16().count();
    (line, character)
}

fn position_to_byte_offset(text: &str, target_line: usize, target_character: usize) -> usize {
    let mut current_line = 0usize;
    let mut line_start = 0usize;
    for (idx, ch) in text.char_indices() {
        if current_line == target_line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    if current_line < target_line {
        return text.len();
    }
    let line_tail = &text[line_start..];
    let line_end_rel = line_tail.find('\n').unwrap_or(line_tail.len());
    let line_text = &line_tail[..line_end_rel];
    let mut utf16_units = 0usize;
    for (idx, ch) in line_text.char_indices() {
        if utf16_units >= target_character {
            return line_start + idx;
        }
        utf16_units += ch.len_utf16();
        if utf16_units > target_character {
            return line_start + idx;
        }
    }
    line_start + line_text.len()
}

fn matches_response_id(value: &JsonValue, id: u32) -> bool {
    value
        .as_object()
        .and_then(|obj| obj.get("id"))
        .and_then(JsonValue::as_u64)
        == Some(id as u64)
}

fn extract_hover_markdown(value: &JsonValue) -> Option<String> {
    let root = value.as_object()?;
    let result = root.get("result")?;
    if matches!(result, JsonValue::Null) {
        return None;
    }
    let contents = result.as_object()?.get("contents")?;
    let markdown = hover_contents_to_markdown(contents);
    if markdown.is_empty() {
        None
    } else {
        Some(markdown)
    }
}

fn request_location_method(
    session: &mut Session,
    method: &str,
    uri: &str,
    line: usize,
    character: usize,
    timeout_ms: usize,
) -> Option<Vec<JsonValue>> {
    let request_id = session.next_id;
    session.next_id += 1;
    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"{}\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\"}},\"position\":{{\"line\":{},\"character\":{}}}}}}}",
        method,
        escape_json(uri),
        line,
        character,
    );
    if !send_json(session, &json) {
        return None;
    }
    let response_text = wait_for_response_text(session, request_id, timeout_ms)?;
    extract_location_result(&response_text)
}

fn request_references(
    session: &mut Session,
    uri: &str,
    line: usize,
    character: usize,
    timeout_ms: usize,
) -> Option<Vec<JsonValue>> {
    let request_id = session.next_id;
    session.next_id += 1;
    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"textDocument/references\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\"}},\"position\":{{\"line\":{},\"character\":{}}},\"context\":{{\"includeDeclaration\":false}}}}}}",
        escape_json(uri),
        line,
        character,
    );
    if !send_json(session, &json) {
        return None;
    }
    let response_text = wait_for_response_text(session, request_id, timeout_ms)?;
    extract_location_result(&response_text)
}

fn request_document_highlights(
    session: &mut Session,
    uri: &str,
    line: usize,
    character: usize,
    timeout_ms: usize,
) -> Option<Vec<JsonValue>> {
    let request_id = session.next_id;
    session.next_id += 1;
    let json = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"method\":\"textDocument/documentHighlight\",\"params\":{{\"textDocument\":{{\"uri\":\"{}\"}},\"position\":{{\"line\":{},\"character\":{}}}}}}}",
        escape_json(uri),
        line,
        character,
    );
    if !send_json(session, &json) {
        return None;
    }
    let response_text = wait_for_response_text(session, request_id, timeout_ms)?;
    extract_location_result(&response_text)
}

fn extract_location_result(message: &str) -> Option<Vec<JsonValue>> {
    let value = parse_json(message)?;
    let root = value.as_object()?;
    let result = root.get("result")?;
    match result {
        JsonValue::Null => Some(Vec::new()),
        JsonValue::Array(items) => Some(items.clone()),
        JsonValue::Object(_) => Some(vec![result.clone()]),
        _ => None,
    }
}

fn location_to_span_for_uri(src: &str, full_path: &str, location: &JsonValue) -> Option<(u32, u32)> {
    let obj = location.as_object()?;
    let uri = obj.get("uri")?.as_str()?;
    if file_uri_to_path(uri) != full_path {
        return None;
    }
    let range = obj.get("range")?.as_object()?;
    let start = range.get("start")?.as_object()?;
    let end = range.get("end")?.as_object()?;
    let start_line = start.get("line")?.as_u64()? as usize;
    let start_char = start.get("character")?.as_u64()? as usize;
    let end_line = end.get("line")?.as_u64()? as usize;
    let end_char = end.get("character")?.as_u64()? as usize;
    let start_offset = position_to_byte_offset(src, start_line, start_char);
    let end_offset = position_to_byte_offset(src, end_line, end_char);
    Some((start_offset as u32, end_offset.saturating_sub(start_offset).max(1) as u32))
}

fn document_highlight_to_span(src: &str, highlight: &JsonValue) -> Option<(u32, u32)> {
    let obj = highlight.as_object()?;
    let range = obj.get("range")?.as_object()?;
    let start = range.get("start")?.as_object()?;
    let end = range.get("end")?.as_object()?;
    let start_line = start.get("line")?.as_u64()? as usize;
    let start_char = start.get("character")?.as_u64()? as usize;
    let end_line = end.get("line")?.as_u64()? as usize;
    let end_char = end.get("character")?.as_u64()? as usize;
    let start_offset = position_to_byte_offset(src, start_line, start_char);
    let end_offset = position_to_byte_offset(src, end_line, end_char);
    Some((start_offset as u32, end_offset.saturating_sub(start_offset).max(1) as u32))
}

fn encode_symbol_relation_record(out: &mut Vec<u8>, offset: u32, length: u32, role: u8) {
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.push(role);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
}

fn hover_contents_to_markdown(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let part = hover_contents_to_markdown(item);
                if !part.is_empty() {
                    parts.push(part);
                }
            }
            parts.join("\n\n")
        }
        JsonValue::Object(obj) => {
            if let Some(value) = obj.get("value").and_then(JsonValue::as_str) {
                if let Some(language) = obj.get("language").and_then(JsonValue::as_str) {
                    return format!("```{language}\n{value}\n```");
                }
                return value.to_string();
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn extract_semantic_data_from_raw(message: &str) -> Option<Vec<JsonValue>> {
    let key = "\"data\":[";
    let start = message.find(key)? + key.len();
    let mut end = start;
    let bytes = message.as_bytes();
    let mut depth = 1i32;
    while end < bytes.len() {
        match bytes[end] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        end += 1;
    }
    if end >= bytes.len() || depth != 0 {
        return None;
    }
    let body = &message[start..end];
    let mut out = Vec::new();
    for part in body.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let num = trimmed.parse::<f64>().ok()?;
        out.push(JsonValue::Number(num));
    }
    Some(out)
}

fn semantic_kind_for_type(token_type: &str) -> u8 {
    match token_type {
        "namespace" => 1,
        "type" | "struct" => 2,
        "class" => 3,
        "enum" => 4,
        "protocol" | "interface" => 5,
        "typeParameter" => 6,
        "parameter" => 7,
        "variable" | "identifier" | "unknown" => 8,
        "property" => 9,
        "enumMember" | "event" => 10,
        "function" => 11,
        "method" => 12,
        "macro" => 13,
        "keyword" => 14,
        "modifier" => 15,
        "comment" => 16,
        "string" => 17,
        "number" => 18,
        "operator" => 19,
        _ => 0,
    }
}

fn semantic_modifiers_mask(bitset: u64, legend: &SemanticLegend) -> u16 {
    let mut out = 0u16;
    for (idx, name) in legend.token_modifiers.iter().enumerate() {
        if (bitset & (1u64 << idx)) == 0 {
            continue;
        }
        out |= match name.as_str() {
            "declaration" => 1 << 0,
            "definition" => 1 << 1,
            "readonly" => 1 << 2,
            "static" => 1 << 3,
            "defaultLibrary" => 1 << 4,
            "deprecated" => 1 << 5,
            "async" => 1 << 6,
            _ => 0,
        };
    }
    out
}

fn encode_semantic_tokens(text: &str, legend: &SemanticLegend, data: &[JsonValue]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut line = 0usize;
    let mut start_char = 0usize;
    let mut i = 0usize;

    while i + 4 < data.len() {
        let delta_line = data[i].as_u64().unwrap_or(0) as usize;
        let delta_start = data[i + 1].as_u64().unwrap_or(0) as usize;
        let length_chars = data[i + 2].as_u64().unwrap_or(0) as usize;
        let token_type_idx = data[i + 3].as_u64().unwrap_or(0) as usize;
        let modifier_bits = data[i + 4].as_u64().unwrap_or(0);
        i += 5;

        if delta_line > 0 {
            line += delta_line;
            start_char = delta_start;
        } else {
            start_char += delta_start;
        }

        let Some(token_type) = legend.token_types.get(token_type_idx) else {
            continue;
        };
        let kind = semantic_kind_for_type(token_type);
        if kind == 0 {
            continue;
        }

        let start = position_to_byte_offset(text, line, start_char);
        let end = position_to_byte_offset(text, line, start_char + length_chars);
        if end <= start {
            continue;
        }

        out.extend_from_slice(&(start as u32).to_le_bytes());
        out.extend_from_slice(&((end - start) as u32).to_le_bytes());
        out.push(kind);
        out.push(0);
        out.extend_from_slice(&semantic_modifiers_mask(modifier_bits, legend).to_le_bytes());
    }

    out
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Clone, Debug)]
enum JsonValue {
    Null,
    Bool,
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    fn as_object(&self) -> Option<&HashMap<String, JsonValue>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) if *value >= 0.0 => Some(*value as u64),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    index: usize,
}

fn parse_json(input: &str) -> Option<JsonValue> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        index: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.index == parser.bytes.len() {
        Some(value)
    } else {
        None
    }
}

impl<'a> JsonParser<'a> {
    fn parse_value(&mut self) -> Option<JsonValue> {
        self.skip_ws();
        let byte = *self.bytes.get(self.index)?;
        match byte {
            b'n' => {
                self.expect_bytes(b"null")?;
                Some(JsonValue::Null)
            }
            b't' => {
                self.expect_bytes(b"true")?;
                Some(JsonValue::Bool)
            }
            b'f' => {
                self.expect_bytes(b"false")?;
                Some(JsonValue::Bool)
            }
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(),
            b'{' => self.parse_object(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Option<JsonValue> {
        self.index += 1;
        let mut out = HashMap::new();
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(JsonValue::Object(out));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.consume(b':').then_some(())?;
            let value = self.parse_value()?;
            out.insert(key, value);
            self.skip_ws();
            if self.consume(b'}') {
                return Some(JsonValue::Object(out));
            }
            self.consume(b',').then_some(())?;
        }
    }

    fn parse_array(&mut self) -> Option<JsonValue> {
        self.index += 1;
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.consume(b']') {
                return Some(JsonValue::Array(out));
            }
            out.push(self.parse_value()?);
            self.skip_ws();
            if self.consume(b']') {
                return Some(JsonValue::Array(out));
            }
            self.consume(b',').then_some(())?;
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        self.consume(b'"').then_some(())?;
        let mut out = String::new();
        while self.index < self.bytes.len() {
            let byte = self.bytes[self.index];
            self.index += 1;
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let esc = *self.bytes.get(self.index)?;
                    self.index += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.parse_u16_hex()?;
                            if let Some(ch) = char::from_u32(code as u32) {
                                out.push(ch);
                            }
                        }
                        _ => return None,
                    }
                }
                _ => out.push(byte as char),
            }
        }
        None
    }

    fn parse_u16_hex(&mut self) -> Option<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = *self.bytes.get(self.index)?;
            self.index += 1;
            value = (value << 4) | from_hex(byte)? as u16;
        }
        Some(value)
    }

    fn parse_number(&mut self) -> Option<JsonValue> {
        let start = self.index;
        if self.bytes[self.index] == b'-' {
            self.index += 1;
        }
        while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_digit() {
            self.index += 1;
        }
        if self.index < self.bytes.len() && self.bytes[self.index] == b'.' {
            self.index += 1;
            while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_digit() {
                self.index += 1;
            }
        }
        if self.index < self.bytes.len() && matches!(self.bytes[self.index], b'e' | b'E') {
            self.index += 1;
            if self.index < self.bytes.len() && matches!(self.bytes[self.index], b'+' | b'-') {
                self.index += 1;
            }
            while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_digit() {
                self.index += 1;
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.index]).ok()?;
        raw.parse::<f64>().ok().map(JsonValue::Number)
    }

    fn skip_ws(&mut self) {
        while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_whitespace() {
            self.index += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_bytes(&mut self, expected: &[u8]) -> Option<()> {
        if self.bytes.get(self.index..self.index + expected.len())? == expected {
            self.index += expected.len();
            Some(())
        } else {
            None
        }
    }
}
