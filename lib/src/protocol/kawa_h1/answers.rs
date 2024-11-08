use crate::{protocol::http::DefaultAnswer, sozu_command::state::ClusterId};
use kawa::{
    h1::NoCallbacks, AsBuffer, Block, BodySize, Buffer, Chunk, Kawa, Kind, Pair, ParsingPhase,
    ParsingPhaseMarker, StatusLine, Store,
};
use sozu_command::proto::command::CustomHttpAnswers;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    rc::Rc,
};

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
    typ: ReplacementType,
}

// TODO: rename for clarity, for instance HttpAnswerTemplate
pub struct Template {
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
        status: u16,
        answer: String,
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
        if let StatusLine::Response { code, .. } = kawa.detached.status_line {
            if code != status {
                return Err(TemplateError::InvalidStatusCode(code));
            }
        } else {
            return Err(TemplateError::InvalidType);
        }
        let buf = kawa.storage.buffer();
        let mut blocks = VecDeque::new();
        let mut header_replacements = Vec::new();
        let mut body_replacements = Vec::new();
        let mut body_size = 0;
        let mut used_once = Vec::new();
        for mut block in kawa.blocks.into_iter() {
            match &mut block {
                Block::ChunkHeader(_) => return Err(TemplateError::UnsupportedStreaming),
                Block::StatusLine | Block::Cookies | Block::Flags(_) => {
                    blocks.push_back(block);
                }
                Block::Header(Pair { key, val }) => {
                    let val_data = val.data(buf);
                    let key_data = key.data(buf);
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
                                    ReplacementType::ContentLength => {
                                        if let Some(b'%') = key_data.first() {
                                            *key = Store::new_slice(buf, &key_data[1..]);
                                        }
                                    }
                                }
                                header_replacements.push(Replacement {
                                    block_index: blocks.len(),
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

/// a set of templates for HTTP answers, meant for one listener to use
pub struct ListenerAnswers {
    /// MovedPermanently
    pub answer_301: Template,
    /// BadRequest
    pub answer_400: Template,
    /// Unauthorized
    pub answer_401: Template,
    /// NotFound
    pub answer_404: Template,
    /// RequestTimeout
    pub answer_408: Template,
    /// PayloadTooLarge
    pub answer_413: Template,
    /// BadGateway
    pub answer_502: Template,
    /// ServiceUnavailable
    pub answer_503: Template,
    /// GatewayTimeout
    pub answer_504: Template,
    /// InsufficientStorage
    pub answer_507: Template,
}

/// templates for HTTP answers, set for one cluster
#[allow(non_snake_case)]
pub struct ClusterAnswers {
    /// ServiceUnavailable
    pub answer_503: Template,
}

pub struct HttpAnswers {
    pub listener_answers: ListenerAnswers, // configurated answers
    pub cluster_custom_answers: HashMap<ClusterId, ClusterAnswers>,
}

// const HEADERS: &str = "Connection: close\r
// Content-Length: 0\r
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
fn default_301() -> String {
    String::from(
        "\
HTTP/1.1 301 Moved Permanently\r
Location: %REDIRECT_LOCATION\r
Connection: close\r
Content-Length: 0\r
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
%Content-Length: %CONTENT_LENGTH\r
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
%Content-Length: %CONTENT_LENGTH\r
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
%Content-Length: %CONTENT_LENGTH\r
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
%Content-Length: %CONTENT_LENGTH\r
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
%Content-Length: %CONTENT_LENGTH\r
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
    pub fn template(status: u16, answer: String) -> Result<Template, (u16, TemplateError)> {
        let length = TemplateVariable {
            name: "CONTENT_LENGTH",
            valid_in_body: false,
            valid_in_header: true,
            typ: ReplacementType::ContentLength,
        };

        let route = TemplateVariable {
            name: "ROUTE",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let request_id = TemplateVariable {
            name: "REQUEST_ID",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let cluster_id = TemplateVariable {
            name: "CLUSTER_ID",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let backend_id = TemplateVariable {
            name: "BACKEND_ID",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let duration = TemplateVariable {
            name: "DURATION",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let capacity = TemplateVariable {
            name: "CAPACITY",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let phase = TemplateVariable {
            name: "PHASE",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };

        let location = TemplateVariable {
            name: "REDIRECT_LOCATION",
            valid_in_body: false,
            valid_in_header: true,
            typ: ReplacementType::VariableOnce(0),
        };
        let message = TemplateVariable {
            name: "MESSAGE",
            valid_in_body: true,
            valid_in_header: false,
            typ: ReplacementType::VariableOnce(0),
        };
        let successfully_parsed = TemplateVariable {
            name: "SUCCESSFULLY_PARSED",
            valid_in_body: true,
            valid_in_header: false,
            typ: ReplacementType::Variable(0),
        };
        let partially_parsed = TemplateVariable {
            name: "PARTIALLY_PARSED",
            valid_in_body: true,
            valid_in_header: false,
            typ: ReplacementType::Variable(0),
        };
        let invalid = TemplateVariable {
            name: "INVALID",
            valid_in_body: true,
            valid_in_header: false,
            typ: ReplacementType::Variable(0),
        };

        match status {
            301 => Template::new(
                301,
                answer,
                &[length, route, request_id, location]
            ),
            400 => Template::new(
                400,
                answer,
                &[length, route, request_id, message, phase, successfully_parsed, partially_parsed, invalid],
            ),
            401 => Template::new(
                401,
                answer,
                &[length, route, request_id]
            ),
            404 => Template::new(
                404,
                answer,
                &[length, route, request_id]
            ),
            408 => Template::new(
                408,
                answer,
                &[length, route, request_id, duration]
            ),
            413 => Template::new(
                413,
                answer,
                &[length, route, request_id, capacity, message, phase],
            ),
            502 => Template::new(
                502,
                answer,
                &[length, route, request_id, cluster_id, backend_id, message, phase, successfully_parsed, partially_parsed, invalid],
            ),
            503 => Template::new(
                503,
                answer,
                &[length, route, request_id, cluster_id, backend_id, message],
            ),
            504 => Template::new(
                504,
                answer,
                &[length, route, request_id, cluster_id, backend_id, duration],
            ),
            507 => Template::new(
                507,
                answer,
                &[length, route, request_id, cluster_id, backend_id, capacity, message, phase],
            ),
            _ => Err(TemplateError::InvalidStatusCode(status)),
        }
        .map_err(|e| (status, e))
    }

    pub fn new(conf: &Option<CustomHttpAnswers>) -> Result<Self, (u16, TemplateError)> {
        Ok(HttpAnswers {
            listener_answers: ListenerAnswers {
                answer_301: Self::template(
                    301,
                    conf.as_ref()
                        .and_then(|c| c.answer_301.clone())
                        .unwrap_or(default_301()),
                )?,
                answer_400: Self::template(
                    400,
                    conf.as_ref()
                        .and_then(|c| c.answer_400.clone())
                        .unwrap_or(default_400()),
                )?,
                answer_401: Self::template(
                    401,
                    conf.as_ref()
                        .and_then(|c| c.answer_401.clone())
                        .unwrap_or(default_401()),
                )?,
                answer_404: Self::template(
                    404,
                    conf.as_ref()
                        .and_then(|c| c.answer_404.clone())
                        .unwrap_or(default_404()),
                )?,
                answer_408: Self::template(
                    408,
                    conf.as_ref()
                        .and_then(|c| c.answer_408.clone())
                        .unwrap_or(default_408()),
                )?,
                answer_413: Self::template(
                    413,
                    conf.as_ref()
                        .and_then(|c| c.answer_413.clone())
                        .unwrap_or(default_413()),
                )?,
                answer_502: Self::template(
                    502,
                    conf.as_ref()
                        .and_then(|c| c.answer_502.clone())
                        .unwrap_or(default_502()),
                )?,
                answer_503: Self::template(
                    503,
                    conf.as_ref()
                        .and_then(|c| c.answer_503.clone())
                        .unwrap_or(default_503()),
                )?,
                answer_504: Self::template(
                    504,
                    conf.as_ref()
                        .and_then(|c| c.answer_504.clone())
                        .unwrap_or(default_504()),
                )?,
                answer_507: Self::template(
                    507,
                    conf.as_ref()
                        .and_then(|c| c.answer_507.clone())
                        .unwrap_or(default_507()),
                )?,
            },
            cluster_custom_answers: HashMap::new(),
        })
    }

    pub fn add_custom_answer(
        &mut self,
        cluster_id: &str,
        answer_503: String,
    ) -> Result<(), (u16, TemplateError)> {
        let answer_503 = Self::template(503, answer_503)?;
        self.cluster_custom_answers
            .insert(cluster_id.to_string(), ClusterAnswers { answer_503 });
        Ok(())
    }

    pub fn remove_custom_answer(&mut self, cluster_id: &str) {
        self.cluster_custom_answers.remove(cluster_id);
    }

    pub fn get(
        &self,
        answer: DefaultAnswer,
        request_id: String,
        cluster_id: Option<&str>,
        backend_id: Option<&str>,
        route: String,
    ) -> DefaultAnswerStream {
        let variables: Vec<Vec<u8>>;
        let mut variables_once: Vec<Vec<u8>>;
        let template = match answer {
            DefaultAnswer::Answer301 { location } => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![location.into()];
                &self.listener_answers.answer_301
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
                &self.listener_answers.answer_400
            }
            DefaultAnswer::Answer401 {} => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![];
                &self.listener_answers.answer_401
            }
            DefaultAnswer::Answer404 {} => {
                variables = vec![route.into(), request_id.into()];
                variables_once = vec![];
                &self.listener_answers.answer_404
            }
            DefaultAnswer::Answer408 { duration } => {
                variables = vec![route.into(), request_id.into(), duration.to_string().into()];
                variables_once = vec![];
                &self.listener_answers.answer_408
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
                &self.listener_answers.answer_413
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
                &self.listener_answers.answer_502
            }
            DefaultAnswer::Answer503 { message } => {
                variables = vec![
                    route.into(),
                    request_id.into(),
                    cluster_id.unwrap_or_default().into(),
                    backend_id.unwrap_or_default().into(),
                ];
                variables_once = vec![message.into()];
                cluster_id
                    .and_then(|id: &str| self.cluster_custom_answers.get(id))
                    .map(|c| &c.answer_503)
                    .unwrap_or_else(|| &self.listener_answers.answer_503)
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
                &self.listener_answers.answer_504
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
                &self.listener_answers.answer_507
            }
        };
        // kawa::debug_kawa(&template.kawa);
        // println!("{template:#?}");
        template.fill(&variables, &mut variables_once)
    }
}
