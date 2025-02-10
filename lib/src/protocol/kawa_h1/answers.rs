use crate::{protocol::http::DefaultAnswer, sozu_command::state::ClusterId};
use kawa::{
    h1::NoCallbacks, AsBuffer, Block, BodySize, Buffer, Chunk, Flags, Kawa, Kind, Pair,
    ParsingPhase, ParsingPhaseMarker, StatusLine, Store,
};
use nom::AsBytes;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
    rc::Rc,
    str::from_utf8_unchecked,
};

use super::parser::compare_no_case;

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

// TODO: rename for clarity, for instance HttpAnswerTemplate
pub struct Template {
    status: u16,
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
            .field("body_replacements", &self.body_replacements)
            .field("header_replacements", &self.header_replacements)
            .field("body_size", &self.body_size)
            .finish()
    }
}

impl Template {
    /// sanitize the template: transform newlines \r (CR) to \r\n (CRLF)
    fn new(
        status: Option<u16>,
        answer: &str,
        variables: &[TemplateVariable],
    ) -> Result<Self, TemplateError> {
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
        if kawa.body_size != BodySize::Empty {
            return Err(TemplateError::InvalidSizeInfo(kawa.body_size));
        }
        let status = if let StatusLine::Response { code, .. } = &kawa.detached.status_line {
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
        for mut block in kawa.blocks.into_iter() {
            match &mut block {
                Block::ChunkHeader(_) => return Err(TemplateError::UnsupportedStreaming),
                Block::Flags(Flags {
                    end_header: true, ..
                }) => {
                    header_replacements.push(Replacement {
                        block_index: blocks.len(),
                        or_elide_header: false,
                        typ: ReplacementType::ContentLength,
                    });
                    blocks.push_back(Block::Header(Pair {
                        key: Store::Static(b"Content-Length"),
                        val: Store::Static(b"PLACEHOLDER"),
                    }));
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
            status,
            keep_alive,
            kawa,
            body_replacements,
            header_replacements,
            body_size,
        })
    }

    fn fill(&self, variables: &[Vec<u8>], variables_once: &mut [Vec<u8>]) -> DefaultAnswerStream {
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
                if pair.val.len() == 0 && replacement.or_elide_header {
                    pair.elide();
                    continue;
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

pub struct HttpAnswers {
    pub cluster_answers: HashMap<ClusterId, BTreeMap<String, Template>>,
    pub listener_answers: BTreeMap<String, Template>,
    pub fallback: Template,
}

// const HEADERS: &str = "Connection: close\r
// Sozu-Id: %REQUEST_ID\r
// \r";
// const STYLE: &str = "<style>
// pre {
//   background: #EEE;
//   padding: 10px;
//   border: 1px solid #AAA;
//   border-radius: 5px;
// }
// </style>";
// const FOOTER: &str = "<footer>This is an automatic answer by Sōzu.</footer>";
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
    \"rewritten_url\": \"%REDIRECT_LOCATION\",
    \"request_id\": \"%REQUEST_ID\"
    \"cluster_id\": \"%CLUSTER_ID\",
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
WWW-Authenticate: %WWW_AUTHENTICATE\r
Cache-Control: no-cache\r
Connection: close\r
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
    \"successfully_parsed\": \"%SUCCESSFULLY_PARSED\",
    \"partially_parsed\": \"%PARTIALLY_PARSED\",
    \"invalid\": \"%INVALID\"
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
<p>Response needed more than %CAPACITY bytes to fit. Parser stopped at phase: %PHASE. %MESSAGE/p>
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

impl HttpAnswers {
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
        let www_authenticate = TemplateVariable {
            name: "WWW_AUTHENTICATE",
            valid_in_body: false,
            valid_in_header: true,
            or_elide_header: true,
            typ: ReplacementType::VariableOnce(0),
        };
        let message = TemplateVariable {
            name: "MESSAGE",
            valid_in_body: true,
            valid_in_header: false,
            or_elide_header: false,
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
        let template_name = TemplateVariable {
            name: "TEMPLATE_NAME",
            valid_in_body: true,
            valid_in_header: true,
            or_elide_header: false,
            typ: ReplacementType::Variable(0),
        };

        match name {
            "301" => Template::new(
                Some(301),
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
                &[route, request_id, cluster_id, location, template_name]
            )
        }
        .map_err(|e| (name.to_owned(), e))
    }

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

    pub fn new(answers: &BTreeMap<String, String>) -> Result<Self, (String, TemplateError)> {
        let mut listener_answers = Self::templates(answers)?;
        let expected_defaults: &[(&str, fn() -> String)] = &[
            ("301", default_301),
            ("400", default_400),
            ("401", default_401),
            ("404", default_404),
            ("408", default_408),
            ("413", default_413),
            ("502", default_502),
            ("503", default_503),
            ("504", default_504),
            ("507", default_507),
        ];
        for (name, default) in expected_defaults {
            listener_answers
                .entry(name.to_string())
                .or_insert_with(|| Self::template(name, &default()).unwrap());
        }
        Ok(HttpAnswers {
            fallback: Self::template("", &fallback()).unwrap(),
            listener_answers,
            cluster_answers: HashMap::new(),
        })
    }

    pub fn add_cluster_answers(
        &mut self,
        cluster_id: &str,
        answers: &BTreeMap<String, String>,
    ) -> Result<(), (String, TemplateError)> {
        self.cluster_answers
            .entry(cluster_id.to_owned())
            .or_default()
            .append(&mut Self::templates(answers)?);
        Ok(())
    }

    pub fn remove_cluster_answers(&mut self, cluster_id: &str) {
        self.cluster_answers.remove(cluster_id);
    }

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
                variables_once = vec![www_authenticate.map(Into::into).unwrap_or_default()];
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
            DefaultAnswer::AnswerCustom { name, location, .. } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    name.into(),
                ];
                variables_once = vec![location.into()];
                unsafe { from_utf8_unchecked(variables[3].as_bytes()) }
            }
        };
        // kawa::debug_kawa(&template.kawa);
        // println!("{template:#?}");
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
