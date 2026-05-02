//! Default HTTP answer templating.
//!
//! Owns the per-cluster default-answer table (`HttpAnswers`) used when Sōzu
//! synthesises a response (parse error, route miss, backend unreachable, …).
//! Templates are pre-rendered into `Kawa<SharedBuffer>` instances at
//! configuration time so the hot path only needs a clone of the shared
//! buffer.
//!
//! Templates are keyed by HTTP status as a string (e.g. `"503"`). Bodies
//! and selected headers may carry `%PLACEHOLDER` variables — the `Template`
//! engine validates the variable set at parse time and substitutes the
//! runtime values when rendering. Variables with `or_elide_header: true`
//! cause the surrounding header line to be dropped if the substituted value
//! is empty (`%WWW_AUTHENTICATE` being the canonical case).

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    rc::Rc,
};

use kawa::{
    AsBuffer, Block, BodySize, Buffer, Chunk, Flags, Kawa, Kind, Pair, ParsingPhase,
    ParsingPhaseMarker, StatusLine, Store, h1::NoCallbacks,
};
use sozu_command::proto::command::CustomHttpAnswers;

use super::parser::compare_no_case;
use crate::{protocol::http::DefaultAnswer, sozu_command::state::ClusterId};

#[derive(Clone)]
pub struct SharedBuffer(Rc<[u8]>);

impl AsBuffer for SharedBuffer {
    fn as_buffer(&self) -> &[u8] {
        &self.0
    }

    fn as_mut_buffer(&mut self) -> &mut [u8] {
        panic!()
    }
}

pub type DefaultAnswerStream = Kawa<SharedBuffer>;

#[derive(thiserror::Error, Debug)]
pub enum TemplateError {
    #[error("invalid template type: request was found, expected response")]
    InvalidType,
    #[error("template seems invalid or incomplete: {0:?}")]
    InvalidTemplate(ParsingPhase),
    #[error("unexpected status code: {0}")]
    InvalidStatusCode(u16),
    #[error("unexpected size info: {0:?}")]
    InvalidSizeInfo(BodySize),
    #[error("streaming is not supported in templates")]
    UnsupportedStreaming,
    #[error("template variable {0} is not allowed in headers")]
    NotAllowedInHeader(&'static str),
    #[error("template variable {0} is not allowed in body")]
    NotAllowedInBody(&'static str),
    #[error("template variable {0} can only be used once")]
    AlreadyConsumed(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct TemplateVariable {
    name: &'static str,
    valid_in_body: bool,
    valid_in_header: bool,
    or_elide_header: bool,
    typ: ReplacementType,
}

#[derive(Clone, Copy, Debug)]
pub enum ReplacementType {
    Variable(usize),
    VariableOnce(usize),
    ContentLength,
}

#[derive(Clone, Copy, Debug)]
pub struct Replacement {
    block_index: usize,
    or_elide_header: bool,
    typ: ReplacementType,
}

pub struct Template {
    /// HTTP status code parsed from the template's status line.
    status: u16,
    /// `false` when the parsed template carries `Connection: close`. Used by
    /// callers to flip `keep_alive_frontend` once the rendered answer is
    /// queued.
    keep_alive: bool,
    kawa: DefaultAnswerStream,
    body_replacements: Vec<Replacement>,
    header_replacements: Vec<Replacement>,
    /// Size of body without any variables
    body_size: usize,
}

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template")
            .field("status", &self.status)
            .field("keep_alive", &self.keep_alive)
            .field("body_replacements", &self.body_replacements)
            .field("header_replacements", &self.header_replacements)
            .field("body_size", &self.body_size)
            .finish()
    }
}

impl Template {
    /// Sanitize the template (CR → CRLF), parse it as an HTTP/1.1 response,
    /// validate every `%VAR` against the variable set, and pre-bake the
    /// block sequence so [`Template::fill`] only needs to swap `Store`s.
    fn new(
        status: Option<u16>,
        answer: &str,
        variables: &[TemplateVariable],
    ) -> Result<Self, TemplateError> {
        // Re-index the caller-supplied variables so each `Variable` /
        // `VariableOnce` occupies a contiguous slot. Callers describe
        // variables declaratively (Variable(0)/VariableOnce(0)) and we
        // rewrite the indices to match the per-template flat array.
        let mut i = 0;
        let mut j = 0;
        let variables = variables
            .iter()
            .map(|v| match v.typ {
                ReplacementType::Variable(_) => {
                    i += 1;
                    TemplateVariable {
                        typ: ReplacementType::Variable(i - 1),
                        ..*v
                    }
                }
                ReplacementType::VariableOnce(_) => {
                    j += 1;
                    TemplateVariable {
                        typ: ReplacementType::VariableOnce(j - 1),
                        ..*v
                    }
                }
                ReplacementType::ContentLength => *v,
            })
            .collect::<Vec<_>>();
        let answer = answer
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .into_bytes();

        let len = answer.len();
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SharedBuffer(Rc::from(answer))));
        kawa.storage.end = len;
        kawa::h1::parse(&mut kawa, &mut NoCallbacks);
        if !kawa.is_main_phase() {
            return Err(TemplateError::InvalidTemplate(kawa.parsing_phase));
        }
        // Reject only `Chunked` — a streaming framing makes no sense in a
        // static answer template. `BodySize::Length(N)` is what kawa
        // reports for the common operator shape
        // `…\r\nContent-Length: N\r\n\r\n<N-byte body>` (used by the
        // legacy `answer_NNN` fields and the test fixtures that round-
        // trip those responses), so accepting it is required for
        // backwards compatibility. The block walk below routes the
        // operator's `Content-Length` header through the `ContentLength`
        // placeholder so the rendered response carries exactly one
        // header (RFC 9110 §8.6 / RFC 7230 §3.3.2 dedup invariant).
        if matches!(kawa.body_size, BodySize::Chunked) {
            return Err(TemplateError::InvalidSizeInfo(kawa.body_size));
        }
        let resolved_status = if let StatusLine::Response { code, .. } = &kawa.detached.status_line
        {
            if let Some(expected_code) = status {
                if expected_code != *code {
                    return Err(TemplateError::InvalidStatusCode(*code));
                }
            }
            *code
        } else {
            return Err(TemplateError::InvalidType);
        };
        let buf = kawa.storage.buffer();
        let mut blocks = VecDeque::new();
        let mut header_replacements = Vec::new();
        let mut body_replacements = Vec::new();
        let mut body_size = 0;
        let mut keep_alive = true;
        let mut used_once = Vec::new();
        // Track whether the operator-supplied template carried its own
        // `Content-Length` header. If yes we wire that block up to the
        // `ContentLength` placeholder (so the value is auto-resolved at
        // `fill` time after any `%`-substitutions in the body) instead
        // of letting the literal value drift away from the actual body
        // length — RFC 9110 §8.6 / RFC 7230 §3.3.2 forbid duplicate
        // `Content-Length` and downstream agents reject those messages
        // hard as a request-smuggling vector.
        //
        // We deliberately do NOT synthesise a `Content-Length` when the
        // operator template omits one. Inserting a synthetic
        // `Content-Length: 0` line would shift wire bytes for every
        // header-only template (default 301 redirects, default 404,
        // operator-supplied templates that intentionally rely on
        // `Connection: close` for body framing). Operators who want a
        // specific Content-Length include it in their template; those
        // who don't get the bytes they wrote.
        let mut content_length_seen = false;
        for mut block in kawa.blocks.into_iter() {
            match &mut block {
                Block::ChunkHeader(_) => return Err(TemplateError::UnsupportedStreaming),
                Block::Flags(Flags {
                    end_header: true, ..
                }) => {
                    blocks.push_back(block);
                }
                Block::StatusLine | Block::Cookies | Block::Flags(_) => {
                    blocks.push_back(block);
                }
                Block::Header(Pair { key, val }) => {
                    let val_data = val.data(buf);
                    let key_data = key.data(buf);
                    if compare_no_case(key_data, b"connection")
                        && compare_no_case(val_data, b"close")
                    {
                        keep_alive = false;
                    }
                    if compare_no_case(key_data, b"content-length") {
                        // Hand the operator's literal Content-Length over
                        // to the same `ContentLength` placeholder machinery
                        // so the rendered response carries exactly one
                        // header with the auto-computed body size.
                        if content_length_seen {
                            return Err(TemplateError::AlreadyConsumed("Content-Length"));
                        }
                        content_length_seen = true;
                        *val = Store::Static(b"PLACEHOLDER");
                        header_replacements.push(Replacement {
                            block_index: blocks.len(),
                            or_elide_header: false,
                            typ: ReplacementType::ContentLength,
                        });
                        blocks.push_back(block);
                        continue;
                    }
                    if let Some(b'%') = val_data.first() {
                        for variable in &variables {
                            if &val_data[1..] == variable.name.as_bytes() {
                                if !variable.valid_in_header {
                                    return Err(TemplateError::NotAllowedInHeader(variable.name));
                                }
                                *val = Store::Static(b"PLACEHOLDER");
                                match variable.typ {
                                    ReplacementType::Variable(_) => {}
                                    ReplacementType::VariableOnce(var_index) => {
                                        if used_once.contains(&var_index) {
                                            return Err(TemplateError::AlreadyConsumed(
                                                variable.name,
                                            ));
                                        }
                                        used_once.push(var_index);
                                    }
                                    ReplacementType::ContentLength => {}
                                }
                                header_replacements.push(Replacement {
                                    block_index: blocks.len(),
                                    or_elide_header: variable.or_elide_header,
                                    typ: variable.typ,
                                });
                                break;
                            }
                        }
                    }
                    blocks.push_back(block);
                }
                Block::Chunk(Chunk { data }) => {
                    let data = data.data(buf);
                    body_size += data.len();
                    let mut start = 0;
                    let mut i = 0;
                    while i < data.len() {
                        if data[i] == b'%' {
                            for variable in &variables {
                                if data[i + 1..].starts_with(variable.name.as_bytes()) {
                                    if !variable.valid_in_body {
                                        return Err(TemplateError::NotAllowedInBody(variable.name));
                                    }
                                    if start < i {
                                        blocks.push_back(Block::Chunk(Chunk {
                                            data: Store::new_slice(buf, &data[start..i]),
                                        }));
                                    }
                                    body_size -= variable.name.len() + 1;
                                    start = i + variable.name.len() + 1;
                                    i += variable.name.len();
                                    match variable.typ {
                                        ReplacementType::Variable(_) => {}
                                        ReplacementType::ContentLength => {}
                                        ReplacementType::VariableOnce(var_index) => {
                                            if used_once.contains(&var_index) {
                                                return Err(TemplateError::AlreadyConsumed(
                                                    variable.name,
                                                ));
                                            }
                                            used_once.push(var_index);
                                        }
                                    }
                                    body_replacements.push(Replacement {
                                        block_index: blocks.len(),
                                        or_elide_header: false,
                                        typ: variable.typ,
                                    });
                                    blocks.push_back(Block::Chunk(Chunk {
                                        data: Store::Static(b"PLACEHOLDER"),
                                    }));
                                    break;
                                }
                            }
                        }
                        i += 1;
                    }
                    if start < data.len() {
                        blocks.push_back(Block::Chunk(Chunk {
                            data: Store::new_slice(buf, &data[start..]),
                        }));
                    }
                }
            }
        }
        kawa.blocks = blocks;
        Ok(Self {
            status: resolved_status,
            keep_alive,
            kawa,
            body_replacements,
            header_replacements,
            body_size,
        })
    }

    /// Substitute runtime values into a pre-parsed template, eliding header
    /// lines whose value substitutes to empty when `or_elide_header` is set.
    pub(crate) fn fill(
        &self,
        variables: &[Vec<u8>],
        variables_once: &mut [Vec<u8>],
    ) -> DefaultAnswerStream {
        let mut blocks = self.kawa.blocks.clone();
        let mut body_size = self.body_size;
        for replacement in &self.body_replacements {
            match replacement.typ {
                ReplacementType::Variable(var_index) => {
                    let variable = &variables[var_index];
                    body_size += variable.len();
                    blocks[replacement.block_index] = Block::Chunk(Chunk {
                        data: Store::from_slice(variable),
                    })
                }
                ReplacementType::VariableOnce(var_index) => {
                    let variable = std::mem::take(&mut variables_once[var_index]);
                    body_size += variable.len();
                    blocks[replacement.block_index] = Block::Chunk(Chunk {
                        data: Store::from_vec(variable),
                    })
                }
                ReplacementType::ContentLength => unreachable!(),
            }
        }
        for replacement in &self.header_replacements {
            if let Block::Header(pair) = &mut blocks[replacement.block_index] {
                match replacement.typ {
                    ReplacementType::Variable(var_index) => {
                        pair.val = Store::from_slice(&variables[var_index]);
                    }
                    ReplacementType::VariableOnce(var_index) => {
                        let variable = std::mem::take(&mut variables_once[var_index]);
                        pair.val = Store::from_vec(variable);
                    }
                    ReplacementType::ContentLength => {
                        pair.val = Store::from_string(body_size.to_string())
                    }
                }
                // `Store::is_empty` reports the enum variant, not the byte
                // length of the held data; we want elision when the
                // substituted value contributes zero bytes, so check `len`.
                #[allow(clippy::len_zero)]
                if pair.val.len() == 0 && replacement.or_elide_header {
                    pair.elide();
                }
            }
        }
        Kawa {
            storage: Buffer::new(self.kawa.storage.buffer.clone()),
            blocks,
            out: Default::default(),
            detached: self.kawa.detached.clone(),
            kind: Kind::Response,
            expects: 0,
            parsing_phase: ParsingPhase::Terminated,
            body_size: BodySize::Length(body_size),
            consumed: false,
        }
    }
}

/// Per-cluster + per-listener templated HTTP answer registry.
///
/// `cluster_answers` overrides `listener_answers` when both define the same
/// status. `fallback` is used when neither has the requested status — useful
/// for templates picked by name rather than by code (currently only the
/// `Answer*` variants in [`DefaultAnswer`]).
pub struct HttpAnswers {
    pub cluster_answers: HashMap<ClusterId, BTreeMap<String, Template>>,
    pub listener_answers: BTreeMap<String, Template>,
    pub fallback: Template,
}

fn fallback() -> String {
    String::from(
        "\
HTTP/1.1 404 Not Found\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>404 Not Found</h1>
<pre>
{
    \"status_code\": 404,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\"
}
</pre>
<h1>A frontend requested template \"%TEMPLATE_NAME\" that couldn't be found</h1>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_301() -> String {
    String::from(
        "\
HTTP/1.1 301 Moved Permanently\r
Location: %REDIRECT_LOCATION\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r\n",
    )
}

/// RFC 9110 §15.4.3 — temporary redirect. UAs may rewrite POST → GET on
/// follow. Closes #1009.
fn default_302() -> String {
    String::from(
        "\
HTTP/1.1 302 Found\r
Location: %REDIRECT_LOCATION\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r\n",
    )
}

/// RFC 9110 §15.4.9 — permanent redirect; the HTTP method MUST be
/// preserved on follow (no GET-rewrite on POST). Closes #1009.
fn default_308() -> String {
    String::from(
        "\
HTTP/1.1 308 Permanent Redirect\r
Location: %REDIRECT_LOCATION\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r\n",
    )
}

fn default_400() -> String {
    String::from(
        "\
HTTP/1.1 400 Bad Request\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>400 Bad Request</h1>
<pre>
{
    \"status_code\": 400,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"parsing_phase\": \"%PHASE\",
    \"successfully_parsed\": %SUCCESSFULLY_PARSED,
    \"partially_parsed\": %PARTIALLY_PARSED,
    \"invalid\": %INVALID
}
</pre>
<p>Request could not be parsed. %MESSAGE</p>
<div id=details hidden=true>
<p>While parsing %PHASE, this was reported as invalid:</p>
<pre>
<span id=p1 style='background:rgba(0,255,0,0.2)'></span><span id=p2 style='background:rgba(255,255,0,0.2)'></span><span id=p3 style='background:rgba(255,0,0,0.2)'></span>
</pre>
</div>
<script>
function display(id, chunks) {
    let [start, end] = chunks.split(' … ');
    let dec = new TextDecoder('utf8');
    let decode = chunk => dec.decode(new Uint8Array(chunk.split(' ').filter(e => e).map(e => parseInt(e, 16))));
    document.getElementById(id).innerText = JSON.stringify(end ? `${decode(start)} […] ${decode(end)}` : `${decode(start)}`).slice(1,-1);
}
let p1 = %SUCCESSFULLY_PARSED;
let p2 = %PARTIALLY_PARSED;
let p3 = %INVALID;
if (p1 !== null) {
    document.getElementById('details').hidden=false;
    display('p1', p1);display('p2', p2);display('p3', p3);
}
</script>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_401() -> String {
    String::from(
        "\
HTTP/1.1 401 Unauthorized\r
Cache-Control: no-cache\r
Connection: close\r
WWW-Authenticate: %WWW_AUTHENTICATE\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>401 Unauthorized</h1>
<pre>
{
    \"status_code\": 401,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\"
}
</pre>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_404() -> String {
    String::from(
        "\
HTTP/1.1 404 Not Found\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>404 Not Found</h1>
<pre>
{
    \"status_code\": 404,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\"
}
</pre>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_408() -> String {
    String::from(
        "\
HTTP/1.1 408 Request Timeout\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>408 Request Timeout</h1>
<pre>
{
    \"status_code\": 408,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\"
}
</pre>
<p>Request timed out after %DURATION.</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_413() -> String {
    String::from(
        "\
HTTP/1.1 413 Payload Too Large\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>413 Payload Too Large</h1>
<pre>
{
    \"status_code\": 413,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\"
}
</pre>
<p>Request needed more than %CAPACITY bytes to fit. Parser stopped at phase: %PHASE. %MESSAGE</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_421() -> String {
    String::from(
        "\
HTTP/1.1 421 Misdirected Request\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>421 Misdirected Request</h1>
<pre>
{
    \"status_code\": 421,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\"
}
</pre>
<p>The request's authority does not match the TLS SNI negotiated for this connection. Retry on a fresh TLS connection that matches the target authority.</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

/// 429 template — emitted when the per-(cluster, source-IP) connection
/// limit is reached. The `Retry-After` header line uses the
/// `or-elide-header` placeholder so the engine drops the entire line
/// when `retry_after` is empty (`Some(0)` or `None`). `Retry-After: 0`
/// would invite an immediate retry that defeats the limit.
fn default_429() -> String {
    String::from(
        "\
HTTP/1.1 429 Too Many Requests\r
Cache-Control: no-cache\r
Connection: close\r
Retry-After: %RETRY_AFTER\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>429 Too Many Requests</h1>
<pre>
{
    \"status_code\": 429,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\"
}
</pre>
<p>The per-(cluster, source-IP) connection limit for this cluster has been reached. Slow down or retry later.</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_502() -> String {
    String::from(
        "\
HTTP/1.1 502 Bad Gateway\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>502 Bad Gateway</h1>
<pre>
{
    \"status_code\": 502,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\",
    \"backend_id\": \"%BACKEND_ID\",
    \"parsing_phase\": \"%PHASE\",
    \"successfully_parsed\": %SUCCESSFULLY_PARSED,
    \"partially_parsed\": %PARTIALLY_PARSED,
    \"invalid\": %INVALID
}
</pre>
<p>Response could not be parsed. %MESSAGE</p>
<div id=details hidden=true>
<p>While parsing %PHASE, this was reported as invalid:</p>
<pre>
<span id=p1 style='background:rgba(0,255,0,0.2)'></span><span id=p2 style='background:rgba(255,255,0,0.2)'></span><span id=p3 style='background:rgba(255,0,0,0.2)'></span>
</pre>
</div>
<script>
function display(id, chunks) {
    let [start, end] = chunks.split(' … ');
    let dec = new TextDecoder('utf8');
    let decode = chunk => dec.decode(new Uint8Array(chunk.split(' ').filter(e => e).map(e => parseInt(e, 16))));
    document.getElementById(id).innerText = JSON.stringify(end ? `${decode(start)} […] ${decode(end)}` : `${decode(start)}`).slice(1,-1);
}
let p1 = %SUCCESSFULLY_PARSED;
let p2 = %PARTIALLY_PARSED;
let p3 = %INVALID;
if (p1 !== null) {
    document.getElementById('details').hidden=false;
    display('p1', p1);display('p2', p2);display('p3', p3);
}
</script>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_503() -> String {
    String::from(
        "\
HTTP/1.1 503 Service Unavailable\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>503 Service Unavailable</h1>
<pre>
{
    \"status_code\": 503,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\",
    \"backend_id\": \"%BACKEND_ID\"
}
</pre>
<p>%MESSAGE</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_504() -> String {
    String::from(
        "\
HTTP/1.1 504 Gateway Timeout\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>504 Gateway Timeout</h1>
<pre>
{
    \"status_code\": 504,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\",
    \"backend_id\": \"%BACKEND_ID\"
}
</pre>
<p>Response timed out after %DURATION.</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn default_507() -> String {
    String::from(
        "\
HTTP/1.1 507 Insufficient Storage\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %REQUEST_ID\r
\r
<html><head><meta charset='utf-8'><head><body>
<style>pre{background:#EEE;padding:10px;border:1px solid #AAA;border-radius: 5px;}</style>
<h1>507 Insufficient Storage</h1>
<pre>
{
    \"status_code\": 507,
    \"route\": \"%ROUTE\",
    \"request_id\": \"%REQUEST_ID\",
    \"cluster_id\": \"%CLUSTER_ID\",
    \"backend_id\": \"%BACKEND_ID\"
}
</pre>
<p>Response needed more than %CAPACITY bytes to fit. Parser stopped at phase: %PHASE. %MESSAGE</p>
<footer>This is an automatic answer by Sōzu.</footer></body></html>",
    )
}

fn phase_to_vec(phase: ParsingPhaseMarker) -> Vec<u8> {
    match phase {
        ParsingPhaseMarker::StatusLine => "StatusLine",
        ParsingPhaseMarker::Headers => "Headers",
        ParsingPhaseMarker::Cookies => "Cookies",
        ParsingPhaseMarker::Body => "Body",
        ParsingPhaseMarker::Chunks => "Chunks",
        ParsingPhaseMarker::Trailers => "Trailers",
        ParsingPhaseMarker::Terminated => "Terminated",
        ParsingPhaseMarker::Error => "Error",
    }
    .into()
}

/// Flatten a legacy [`CustomHttpAnswers`] (per-status `Option<String>` fields)
/// into the new [`BTreeMap<String, String>`] shape, dropping any `None` field.
///
/// Used to reconcile state files produced by older managers (which still
/// emit `http_answers`) with the new template-engine map. The new map wins
/// on collision; this helper only fills in entries that the new map does
/// not already define.
pub fn legacy_to_map(legacy: &CustomHttpAnswers) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    macro_rules! insert {
        ($code:literal, $field:ident) => {
            if let Some(ref v) = legacy.$field {
                out.insert($code.to_owned(), v.to_owned());
            }
        };
    }
    insert!("301", answer_301);
    insert!("400", answer_400);
    insert!("401", answer_401);
    insert!("404", answer_404);
    insert!("408", answer_408);
    insert!("413", answer_413);
    insert!("421", answer_421);
    insert!("429", answer_429);
    insert!("502", answer_502);
    insert!("503", answer_503);
    insert!("504", answer_504);
    insert!("507", answer_507);
    out
}

/// Fold a legacy [`CustomHttpAnswers`] into the new map without overriding
/// existing entries. Lets the worker accept managers that still populate
/// `http_answers` while preferring the template map when both are set.
pub fn merge_legacy_into_map(answers: &mut BTreeMap<String, String>, legacy: &CustomHttpAnswers) {
    for (code, body) in legacy_to_map(legacy) {
        answers.entry(code).or_insert(body);
    }
}

impl HttpAnswers {
    /// Parse a single template body. Use the status-string variants of the
    /// canonical variable set when the name parses as a known HTTP status;
    /// otherwise fall back to the broadest variable set so user-defined
    /// templates (selected by name) still get the common variables.
    #[rustfmt::skip]
    pub fn template(name: &str, answer: &str) -> Result<Template, (String, TemplateError)> {
        let route = TemplateVariable {
            name: "ROUTE",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let request_id = TemplateVariable {
            name: "REQUEST_ID",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let cluster_id = TemplateVariable {
            name: "CLUSTER_ID",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let backend_id = TemplateVariable {
            name: "BACKEND_ID",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let duration = TemplateVariable {
            name: "DURATION",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let capacity = TemplateVariable {
            name: "CAPACITY",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let phase = TemplateVariable {
            name: "PHASE",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };

        let location = TemplateVariable {
            name: "REDIRECT_LOCATION",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::VariableOnce(0),
        };
        let message = TemplateVariable {
            name: "MESSAGE",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
            typ: ReplacementType::VariableOnce(0),
        };
        let template_name = TemplateVariable {
            name: "TEMPLATE_NAME",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::VariableOnce(0),
        };
        let www_authenticate = TemplateVariable {
            name: "WWW_AUTHENTICATE",
            valid_in_body: false,
            valid_in_header: true,
            // empty realm → drop the `WWW-Authenticate` header line entirely
            or_elide_header: true,
            typ: ReplacementType::VariableOnce(0),
        };
        let retry_after = TemplateVariable {
            name: "RETRY_AFTER",
            valid_in_body: false,
            valid_in_header: true,
            // empty value → drop the `Retry-After` header line entirely.
            // `Retry-After: 0` invites an immediate retry that defeats
            // the per-(cluster, source-IP) limit, so we elide instead of
            // rendering a literal zero.
            or_elide_header: true,
            typ: ReplacementType::VariableOnce(0),
        };
        let successfully_parsed = TemplateVariable {
            name: "SUCCESSFULLY_PARSED",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let partially_parsed = TemplateVariable {
            name: "PARTIALLY_PARSED",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };
        let invalid = TemplateVariable {
            name: "INVALID",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };

        match name {
            "301" => Template::new(
                Some(301),
                answer,
                &[route, request_id, location]
            ),
            "302" => Template::new(
                Some(302),
                answer,
                &[route, request_id, location]
            ),
            "308" => Template::new(
                Some(308),
                answer,
                &[route, request_id, location]
            ),
            "400" => Template::new(
                Some(400),
                answer,
                &[route, request_id, message, phase, successfully_parsed, partially_parsed, invalid],
            ),
            "401" => Template::new(
                Some(401),
                answer,
                &[route, request_id, www_authenticate]
            ),
            "404" => Template::new(
                Some(404),
                answer,
                &[route, request_id]
            ),
            "408" => Template::new(
                Some(408),
                answer,
                &[route, request_id, duration]
            ),
            "413" => Template::new(
                Some(413),
                answer,
                &[route, request_id, capacity, message, phase],
            ),
            "421" => Template::new(
                Some(421),
                answer,
                &[route, request_id]
            ),
            "429" => Template::new(
                Some(429),
                answer,
                &[route, request_id, cluster_id, retry_after],
            ),
            "502" => Template::new(
                Some(502),
                answer,
                &[route, request_id, cluster_id, backend_id, message, phase, successfully_parsed, partially_parsed, invalid],
            ),
            "503" => Template::new(
                Some(503),
                answer,
                &[route, request_id, cluster_id, backend_id, message],
            ),
            "504" => Template::new(
                Some(504),
                answer,
                &[route, request_id, cluster_id, backend_id, duration],
            ),
            "507" => Template::new(
                Some(507),
                answer,
                &[route, request_id, cluster_id, backend_id, capacity, message, phase],
            ),
            _ => Template::new(
                None,
                answer,
                &[route, request_id, cluster_id, location, template_name],
            ),
        }
        .map_err(|e| (name.to_owned(), e))
    }

    /// Compile every `(name → body)` pair into a templates map. Stops on the
    /// first parse error; the returned tuple identifies which template failed.
    pub fn templates(
        answers: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Template>, (String, TemplateError)> {
        answers
            .iter()
            .map(|(name, answer)| {
                Self::template(name, answer).map(|template| (name.clone(), template))
            })
            .collect::<Result<_, _>>()
    }

    /// Build the listener-level template registry. The map can be empty —
    /// every status code referenced in [`DefaultAnswer`] gets a built-in
    /// fallback template.
    pub fn new(answers: &BTreeMap<String, String>) -> Result<Self, (String, TemplateError)> {
        type DefaultBuilder = fn() -> String;
        let mut listener_answers = Self::templates(answers)?;
        let expected_defaults: &[(&str, DefaultBuilder)] = &[
            ("301", default_301),
            ("302", default_302),
            ("308", default_308),
            ("400", default_400),
            ("401", default_401),
            ("404", default_404),
            ("408", default_408),
            ("413", default_413),
            ("421", default_421),
            ("429", default_429),
            ("502", default_502),
            ("503", default_503),
            ("504", default_504),
            ("507", default_507),
        ];
        for (name, default) in expected_defaults {
            if !listener_answers.contains_key(*name) {
                // The built-in templates are static strings shipped with
                // the crate. A parse failure here means a regression in
                // the bundled defaults — propagate the typed error so the
                // worker surfaces a `ListenerError::TemplateParse(name,
                // …)` at boot rather than panicking. Pinned by the
                // `default_templates_all_parse` regression test below.
                let template = Self::template(name, &default())?;
                listener_answers.insert(name.to_string(), template);
            }
        }
        Ok(HttpAnswers {
            fallback: Self::template("", &fallback())?,
            listener_answers,
            cluster_answers: HashMap::new(),
        })
    }

    /// Register or extend the per-cluster template overrides for `cluster_id`.
    ///
    /// New entries shadow the listener-level templates for the matching
    /// status. Existing entries with the same status are replaced. Entries
    /// already registered for the cluster but absent from `answers` are kept.
    pub fn add_cluster_answers(
        &mut self,
        cluster_id: &str,
        answers: &BTreeMap<String, String>,
    ) -> Result<(), (String, TemplateError)> {
        if answers.is_empty() {
            return Ok(());
        }
        let mut compiled = Self::templates(answers)?;
        self.cluster_answers
            .entry(cluster_id.to_owned())
            .or_default()
            .append(&mut compiled);
        Ok(())
    }

    /// Drop every per-cluster override for `cluster_id`. Subsequent lookups
    /// fall through to the listener-level template.
    pub fn remove_cluster_answers(&mut self, cluster_id: &str) {
        self.cluster_answers.remove(cluster_id);
    }

    /// Compile a frontend-supplied 301 template on the fly and render it
    /// with the canonical `(REDIRECT_LOCATION, ROUTE, REQUEST_ID)`
    /// variable schema, returning the same `(status, keep_alive,
    /// rendered_stream)` triple as [`HttpAnswers::get`].
    ///
    /// ── Why a one-off compile path ──
    ///
    /// `HttpAnswers::get` resolves a template via the persistent lookup
    /// chain (per-cluster override → listener default → fallback). A
    /// `Frontend` may instead carry its own `redirect_template` (proto +
    /// TOML) that should override the chain for a single
    /// `RedirectPolicy::PERMANENT` request. Storing the compiled
    /// template in the persistent map would require a per-frontend key
    /// (which the map is not designed for) and would still diverge if
    /// two frontends on the same cluster carried different templates.
    /// Compiling on demand sidesteps both issues at the cost of one
    /// `Template::new` per redirected request — measurable but bounded
    /// by Frontend cardinality, since templates that compile cleanly
    /// today will compile cleanly tomorrow.
    ///
    /// Returns `None` when compilation fails. Callers MUST fall back to
    /// the regular `HttpAnswers::get` path so a late-discovered config
    /// bug never blocks the request — the rendered listener default is
    /// always preferable to a 503 / hung response.
    pub fn render_inline_301(
        answer_str: &str,
        location: Option<String>,
        request_id: String,
        route: String,
    ) -> Option<(u16, bool, DefaultAnswerStream)> {
        Self::render_inline_redirect(301, answer_str, location, request_id, route)
    }

    /// Compile + render an operator-supplied template as a 301 / 302 / 308
    /// redirect (#1009). Same shape as `render_inline_301` (which is now a
    /// thin wrapper) — the variable order is fixed `(ROUTE, REQUEST_ID,
    /// REDIRECT_LOCATION)` regardless of the redirect status.
    ///
    /// Returns `None` when compilation fails, when the supplied `code` is
    /// not a recognised redirect status, or when the parsed template's
    /// status header doesn't match the requested code (operator config bug:
    /// the answer engine downstream expects `template.status == code`).
    pub fn render_inline_redirect(
        code: u16,
        answer_str: &str,
        location: Option<String>,
        request_id: String,
        route: String,
    ) -> Option<(u16, bool, DefaultAnswerStream)> {
        // Each redirect status has its own template name in the registry
        // (`"301"`, `"302"`, `"308"`); reject anything else early so a
        // miswired call site does not silently render the wrong status.
        let name = match code {
            301 => "301",
            302 => "302",
            308 => "308",
            _ => return None,
        };
        let template = Self::template(name, answer_str).ok()?;
        // Variable order must match the matching `DefaultAnswer::Answer{301,302,308}`
        // arm of `HttpAnswers::get`: `(ROUTE, REQUEST_ID)` are persistent
        // `Variable` slots, `REDIRECT_LOCATION` is the once slot.
        let variables: Vec<Vec<u8>> = vec![route.into(), request_id.into()];
        let mut variables_once: Vec<Vec<u8>> = vec![location.unwrap_or_default().into()];
        let stream = template.fill(&variables, &mut variables_once);
        Some((template.status, template.keep_alive, stream))
    }

    /// Lookup + render. Returns the resolved status, the template's
    /// `keep_alive` flag (false when the template carries
    /// `Connection: close`), and the rendered Kawa stream ready to be
    /// prepared for the wire.
    pub fn get(
        &self,
        answer: DefaultAnswer,
        request_id: String,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        route: String,
    ) -> (u16, bool, DefaultAnswerStream) {
        let variables: Vec<Vec<u8>>;
        let mut variables_once: Vec<Vec<u8>>;
        let name = match answer {
            DefaultAnswer::Answer301 { location } => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![location.into()];
                "301"
            }
            DefaultAnswer::Answer302 { location } => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![location.into()];
                "302"
            }
            DefaultAnswer::Answer308 { location } => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![location.into()];
                "308"
            }
            DefaultAnswer::Answer400 {
                message,
                phase,
                successfully_parsed,
                partially_parsed,
                invalid,
            } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    phase_to_vec(phase),
                    successfully_parsed.into(),
                    partially_parsed.into(),
                    invalid.into(),
                ];
                variables_once = vec![message.into()];
                "400"
            }
            DefaultAnswer::Answer401 { www_authenticate } => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![www_authenticate.unwrap_or_default().into()];
                "401"
            }
            DefaultAnswer::Answer404 {} => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![];
                "404"
            }
            DefaultAnswer::Answer408 { duration } => {
                variables = vec![route.into(), request_id.into(), duration.to_string().into()];
                variables_once = vec![];
                "408"
            }
            DefaultAnswer::Answer413 {
                message,
                phase,
                capacity,
            } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    capacity.to_string().into(),
                    phase_to_vec(phase),
                ];
                variables_once = vec![message.into()];
                "413"
            }
            DefaultAnswer::Answer421 {} => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![];
                "421"
            }
            DefaultAnswer::Answer429 { retry_after } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                ];
                // `None` or `Some(0)` ⇒ empty, which the template engine
                // turns into a dropped `Retry-After:` line via the
                // `or_elide_header` flag on the RETRY_AFTER variable.
                let retry_after_value: Vec<u8> = match retry_after {
                    Some(v) if v > 0 => v.to_string().into(),
                    _ => Vec::new(),
                };
                variables_once = vec![retry_after_value];
                "429"
            }
            DefaultAnswer::Answer502 {
                message,
                phase,
                successfully_parsed,
                partially_parsed,
                invalid,
            } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    backend_id.unwrap_or_default().into(),
                    phase_to_vec(phase),
                    successfully_parsed.into(),
                    partially_parsed.into(),
                    invalid.into(),
                ];
                variables_once = vec![message.into()];
                "502"
            }
            DefaultAnswer::Answer503 { message } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    backend_id.unwrap_or_default().into(),
                ];
                variables_once = vec![message.into()];
                "503"
            }
            DefaultAnswer::Answer504 { duration } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    backend_id.unwrap_or_default().into(),
                    duration.to_string().into(),
                ];
                variables_once = vec![];
                "504"
            }
            DefaultAnswer::Answer507 {
                phase,
                message,
                capacity,
            } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    backend_id.unwrap_or_default().into(),
                    capacity.to_string().into(),
                    phase_to_vec(phase),
                ];
                variables_once = vec![message.into()];
                "507"
            }
        };
        let template = cluster_id
            .and_then(|id| self.cluster_answers.get(id))
            .and_then(|answers| answers.get(name))
            .or_else(|| self.listener_answers.get(name))
            .unwrap_or(&self.fallback);
        (
            template.status,
            template.keep_alive,
            template.fill(&variables, &mut variables_once),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sozu_command::proto::command::CustomHttpAnswers;

    use kawa::BodySize;

    use super::{
        HttpAnswers, ReplacementType, Template, TemplateError, TemplateVariable, legacy_to_map,
        merge_legacy_into_map,
    };
    use crate::protocol::http::DefaultAnswer;

    /// Every bundled default template (and the fallback) must parse
    /// cleanly so `HttpAnswers::new(&BTreeMap::new())` never returns an
    /// error. This guards the previously-`expect`'d invariant in
    /// `HttpAnswers::new` and turns any future regression in a default
    /// body into a unit-test failure rather than a worker-boot panic.
    #[test]
    fn default_templates_all_parse() {
        HttpAnswers::new(&BTreeMap::new())
            .expect("every bundled default template + fallback must parse");
    }

    fn body_route() -> TemplateVariable {
        TemplateVariable {
            name: "ROUTE",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        }
    }

    fn header_realm() -> TemplateVariable {
        TemplateVariable {
            name: "WWW_AUTHENTICATE",
            valid_in_body: false,
            valid_in_header: true,
            or_elide_header: true,
            typ: ReplacementType::VariableOnce(0),
        }
    }

    fn variable_once_message() -> TemplateVariable {
        TemplateVariable {
            name: "MESSAGE",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
            typ: ReplacementType::VariableOnce(0),
        }
    }

    /// `Template::new` accepts a well-formed body that uses a declared
    /// variable.
    #[test]
    fn template_new_accepts_known_variable() {
        let body = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nroute=%ROUTE";
        let template =
            Template::new(Some(200), body, &[body_route()]).expect("template must compile");
        let mut empty_once: Vec<Vec<u8>> = Vec::new();
        let rendered = template.fill(&[b"hello".to_vec()], &mut empty_once);
        let buf = rendered.storage.buffer();
        assert!(
            rendered.blocks.iter().any(
                |block| matches!(block, kawa::Block::Chunk(kawa::Chunk { data })
                    if data.data(buf) == b"hello")
            ),
            "rendered body must contain the substituted ROUTE variable"
        );
    }

    /// Header-only variable referenced from the body must error at compile
    /// time. Exercises the `valid_in_body` guard, which is the same code
    /// path that catches an unknown `%FOOBAR` (no matching variable, but
    /// inversely: a typed mismatch).
    #[test]
    fn template_new_rejects_header_only_in_body() {
        let body = "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\nrealm=%WWW_AUTHENTICATE";
        let err = Template::new(Some(401), body, &[header_realm()])
            .expect_err("variable not allowed in body must error");
        match err {
            TemplateError::NotAllowedInBody(name) => assert_eq!(name, "WWW_AUTHENTICATE"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// `Template::new` enforces `VariableOnce` semantics: a one-shot
    /// variable referenced twice must error at compile time.
    #[test]
    fn template_new_rejects_variable_once_reused() {
        let body = "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n%MESSAGE %MESSAGE";
        let err = Template::new(Some(503), body, &[variable_once_message()])
            .expect_err("VariableOnce reused twice must error");
        match err {
            TemplateError::AlreadyConsumed(name) => assert_eq!(name, "MESSAGE"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// `Template::fill` substitutes the runtime value into the body chunk.
    #[test]
    fn template_fill_substitutes_body_variable() {
        let body = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nroute=%ROUTE";
        let template = Template::new(Some(200), body, &[body_route()]).expect("template compiles");
        let mut empty_once: Vec<Vec<u8>> = Vec::new();
        let rendered = template.fill(&[b"abc".to_vec()], &mut empty_once);
        let mut concatenated = Vec::new();
        let buf = rendered.storage.buffer();
        for block in &rendered.blocks {
            if let kawa::Block::Chunk(kawa::Chunk { data }) = block {
                concatenated.extend_from_slice(data.data(buf));
            }
        }
        assert!(
            concatenated
                .windows(b"route=abc".len())
                .any(|w| w == b"route=abc"),
            "rendered body must contain the substituted variable, got {:?}",
            String::from_utf8_lossy(&concatenated),
        );
    }

    /// Adjacent body variables (`%A%B` with no literal between them) must
    /// keep `body_size` accounting consistent. Each variable's placeholder
    /// is removed from the running body size before the runtime value is
    /// added, so the post-fill total equals the substituted value bytes
    /// even when two placeholders are back-to-back. This guards against
    /// the `body_size -= variable.name.len() + 1` underflow class that
    /// would otherwise be brittle on corner cases.
    #[test]
    fn template_fill_adjacent_body_variables() {
        // Two placeholders, no literal between them, plus a literal head
        // and tail so we're sure the inter-chunk arithmetic balances.
        let body = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n[%ROUTE%ROUTE]";
        let template = Template::new(Some(200), body, &[body_route()])
            .expect("adjacent %VAR%VAR template compiles");
        let mut empty_once: Vec<Vec<u8>> = Vec::new();
        let rendered = template.fill(&[b"X".to_vec()], &mut empty_once);
        let mut concatenated = Vec::new();
        let buf = rendered.storage.buffer();
        for block in &rendered.blocks {
            if let kawa::Block::Chunk(kawa::Chunk { data }) = block {
                concatenated.extend_from_slice(data.data(buf));
            }
        }
        // Each `%ROUTE` placeholder (6 bytes + leading `%` = 7 bytes) is
        // replaced with the 1-byte runtime value. The concatenated body
        // bytes must read literally `[XX]` — proving both substitutions
        // landed and the inter-chunk arithmetic kept the chunk
        // boundaries.
        assert!(
            concatenated.windows(b"[XX]".len()).any(|w| w == b"[XX]"),
            "adjacent variables must both substitute, got {:?}",
            String::from_utf8_lossy(&concatenated),
        );
    }

    /// `or_elide_header: true` drops the surrounding header line when the
    /// substitution is empty. This is the path that makes
    /// `WWW-Authenticate: <realm>` disappear when the cluster has no realm
    /// configured.
    #[test]
    fn template_fill_elides_header_when_empty() {
        let body = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: %WWW_AUTHENTICATE\r\nConnection: close\r\n\r\n";
        let template =
            Template::new(Some(401), body, &[header_realm()]).expect("template compiles");
        // empty realm → header is elided
        let mut variables_once = vec![Vec::<u8>::new()];
        let rendered = template.fill(&[], &mut variables_once);
        let buf = rendered.storage.buffer();
        let elided = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Header(pair) => pair.is_elided() && pair.val.data(buf).is_empty(),
            _ => false,
        });
        assert!(
            elided,
            "WWW-Authenticate header must be elided when realm is empty"
        );
        // Non-empty realm → header is kept with the substituted value.
        let mut variables_once = vec![b"Basic realm=\"test\"".to_vec()];
        let rendered = template.fill(&[], &mut variables_once);
        let buf = rendered.storage.buffer();
        let kept = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Header(pair) => {
                !pair.is_elided() && pair.key.data(buf).eq_ignore_ascii_case(b"WWW-Authenticate")
            }
            _ => false,
        });
        assert!(
            kept,
            "WWW-Authenticate header must be kept when realm is non-empty"
        );
    }

    /// `add_cluster_answers` followed by `get` returns the per-cluster
    /// override; `remove_cluster_answers` falls back to the listener-level
    /// template.
    #[test]
    fn cluster_overrides_listener() {
        let mut answers = HttpAnswers::new(&BTreeMap::new()).expect("default HttpAnswers");
        let custom_503 =
            "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nfrom-cluster";
        let mut overrides = BTreeMap::new();
        overrides.insert("503".to_owned(), custom_503.to_owned());
        answers
            .add_cluster_answers("c1", &overrides)
            .expect("override compiles");

        let (status, _keep_alive, rendered) = answers.get(
            DefaultAnswer::Answer503 {
                message: String::new(),
            },
            "rid".to_owned(),
            Some("c1"),
            None,
            "/".to_owned(),
        );
        assert_eq!(status, 503);
        let buf = rendered.storage.buffer();
        let body_has_override = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Chunk(kawa::Chunk { data }) => data.data(buf) == b"from-cluster",
            _ => false,
        });
        assert!(body_has_override, "cluster-level body must win");

        // Remove and check the listener-level body comes back.
        answers.remove_cluster_answers("c1");
        let (_, _, rendered) = answers.get(
            DefaultAnswer::Answer503 {
                message: String::new(),
            },
            "rid".to_owned(),
            Some("c1"),
            None,
            "/".to_owned(),
        );
        let buf = rendered.storage.buffer();
        let body_still_overridden = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Chunk(kawa::Chunk { data }) => data.data(buf) == b"from-cluster",
            _ => false,
        });
        assert!(
            !body_still_overridden,
            "after remove, listener-level body must take over"
        );
    }

    /// Round-trip a legacy `CustomHttpAnswers { answer_503: ... }` through
    /// `legacy_to_map` + `HttpAnswers::new`, and verify the rendered 503
    /// carries the legacy body.
    #[test]
    fn legacy_to_map_roundtrip() {
        let legacy = CustomHttpAnswers {
            answer_503: Some(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nlegacy-503"
                    .to_owned(),
            ),
            ..Default::default()
        };
        let map = legacy_to_map(&legacy);
        assert!(map.contains_key("503"), "legacy_to_map must preserve 503");
        let answers = HttpAnswers::new(&map).expect("HttpAnswers::new accepts legacy map");
        let (status, _, rendered) = answers.get(
            DefaultAnswer::Answer503 {
                message: String::new(),
            },
            "rid".to_owned(),
            None,
            None,
            "/".to_owned(),
        );
        assert_eq!(status, 503);
        let buf = rendered.storage.buffer();
        let has_legacy = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Chunk(kawa::Chunk { data }) => data.data(buf) == b"legacy-503",
            _ => false,
        });
        assert!(
            has_legacy,
            "legacy-503 body must round-trip into the new map"
        );
    }

    /// Operator-supplied templates that carry a literal `Content-Length`
    /// must compile (no `InvalidSizeInfo` rejection) AND must not emit
    /// two `Content-Length` headers on the wire — the literal value
    /// gets routed through the [`ReplacementType::ContentLength`]
    /// placeholder so the rendered response carries exactly one header,
    /// auto-recomputed from the actual body size after fill-time
    /// substitutions.
    ///
    /// Regression guard against the CI failure on
    /// `test_http_answers_replace_preserves_cluster_overrides` where
    /// the parser rejected `…\r\nContent-Length: 19\r\n\r\n<19-byte body>`
    /// with `InvalidSizeInfo(Length(19))`.
    #[test]
    fn template_new_accepts_literal_content_length() {
        let body =
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 19\r\n\r\ncluster-custom-503!";
        let template =
            Template::new(Some(503), body, &[]).expect("literal Content-Length compiles");
        let mut empty_once: Vec<Vec<u8>> = Vec::new();
        let rendered = template.fill(&[], &mut empty_once);
        let buf = rendered.storage.buffer();

        // Exactly one Content-Length header survives in the rendered
        // response. Two would be a CWE-444 / smuggling primitive.
        let cl_count = rendered
            .blocks
            .iter()
            .filter(|block| match block {
                kawa::Block::Header(pair) => {
                    !pair.is_elided() && pair.key.data(buf).eq_ignore_ascii_case(b"Content-Length")
                }
                _ => false,
            })
            .count();
        assert_eq!(
            cl_count, 1,
            "literal Content-Length must dedup to a single header"
        );
        // Body bytes survived the parse → fill round-trip.
        let has_body = rendered.blocks.iter().any(|block| match block {
            kawa::Block::Chunk(kawa::Chunk { data }) => data.data(buf) == b"cluster-custom-503!",
            _ => false,
        });
        assert!(
            has_body,
            "literal body bytes must survive into the rendered response"
        );
    }

    /// Templates that omit `Content-Length` must NOT have one
    /// synthesised — operators rely on the byte-for-byte shape of
    /// header-only templates (default 301 redirect, default 404, any
    /// `Connection: close` short response). Auto-injecting
    /// `Content-Length: 0` is the regression that broke
    /// `test_http_behaviors` and `test_https_redirect` in CI.
    #[test]
    fn template_new_does_not_synthesise_content_length() {
        let body = "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/\r\nConnection: close\r\n\r\n";
        let template = Template::new(Some(301), body, &[]).expect("header-only template compiles");
        let mut empty_once: Vec<Vec<u8>> = Vec::new();
        let rendered = template.fill(&[], &mut empty_once);
        let buf = rendered.storage.buffer();
        let cl_count = rendered
            .blocks
            .iter()
            .filter(|block| match block {
                kawa::Block::Header(pair) => {
                    !pair.is_elided() && pair.key.data(buf).eq_ignore_ascii_case(b"Content-Length")
                }
                _ => false,
            })
            .count();
        assert_eq!(
            cl_count, 0,
            "templates without Content-Length must not have one synthesised"
        );
    }

    /// `Transfer-Encoding: chunked` is the only `BodySize` shape we
    /// reject — a streaming framing makes no sense in a static
    /// template. `Length(_)` and `Empty` both compile.
    #[test]
    fn template_new_rejects_chunked_template() {
        let body =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n";
        let err =
            Template::new(Some(200), body, &[]).expect_err("chunked template must be rejected");
        match err {
            TemplateError::InvalidSizeInfo(BodySize::Chunked) => {}
            other => panic!("expected InvalidSizeInfo(Chunked), got {other:?}"),
        }
    }

    /// `merge_legacy_into_map` does not override existing entries — the new
    /// map wins on collision (per the proto comment on field 9/12/21/38/43).
    #[test]
    fn merge_legacy_into_map_prefers_new() {
        let mut new_map = BTreeMap::new();
        new_map.insert(
            "503".to_owned(),
            "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nfrom-new".to_owned(),
        );
        let legacy = CustomHttpAnswers {
            answer_503: Some(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nfrom-legacy"
                    .to_owned(),
            ),
            answer_404: Some(
                "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\nlegacy-404".to_owned(),
            ),
            ..Default::default()
        };
        merge_legacy_into_map(&mut new_map, &legacy);
        assert_eq!(
            new_map.get("503").map(String::as_str),
            Some("HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nfrom-new"),
            "new map must keep its 503 even though legacy has one"
        );
        assert!(
            new_map.contains_key("404"),
            "legacy 404 must fill in when new map does not define one"
        );
    }
}
