use crate::{protocol::http::DefaultAnswer, sozu_command::state::ClusterId};
use kawa::{
    h1::NoCallbacks, AsBuffer, Block, BodySize, Buffer, Chunk, Kawa, Kind, Pair, ParsingPhase,
    StatusLine, Store,
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
    // Size of body without any variables
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
    pub fn new(
        status: u16,
        answer: Vec<u8>,
        variables: &[TemplateVariable],
    ) -> Result<Self, (u16, TemplateError)> {
        Self::_new(status, answer, variables).map_err(|e| (status, e))
    }

    fn _new(
        status: u16,
        answer: Vec<u8>,
        variables: &[TemplateVariable],
    ) -> Result<Self, TemplateError> {
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
                        for variable in variables {
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
                            for variable in variables {
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

    fn fill(&self, variables: &[&[u8]], variables_once: &mut [Vec<u8>]) -> DefaultAnswerStream {
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
                        pair.val = Store::from_slice(variables[var_index]);
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
pub struct RawAnswers {
    /// MovedPermanently
    pub answer_301: Vec<u8>,
    /// BadRequest
    pub answer_400: Vec<u8>,
    /// Unauthorized
    pub answer_401: Vec<u8>,
    /// NotFound
    pub answer_404: Vec<u8>,
    /// RequestTimeout
    pub answer_408: Vec<u8>,
    /// PayloadTooLarge
    pub answer_413: Vec<u8>,
    /// BadGateway
    pub answer_502: Vec<u8>,
    /// ServiceUnavailable
    pub answer_503: Vec<u8>,
    /// GatewayTimeout
    pub answer_504: Vec<u8>,
    /// InsufficientStorage
    pub answer_507: Vec<u8>,
}

// TODO: rename for clarity. These are not default answers,
pub struct DefaultAnswers {
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

#[allow(non_snake_case)]
pub struct CustomAnswers {
    /// ServiceUnavailable
    pub answer_503: Template,
}

pub struct HttpAnswers {
    pub default: DefaultAnswers,
    // TODO: rename to custom_503_answers, bring its type to HashMap<ClusterId, Template>
    pub custom: HashMap<ClusterId, CustomAnswers>,
}

fn default_301() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 301 Moved Permanently\r
Location: %REDIRECT_LOCATION\r
Connection: close\r
Content-Length: 0\r
Sozu-Id: %SOZU_ID\r
\r\n"[..],
    )
}

fn default_400() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 400 Bad Request\r
Cache-Control: no-cache\r
Connection: close\r
%Content-Length: %CONTENT_LENGTH\r
Sozu-Id: %SOZU_ID\r
\r
<h1>Sozu automatic 400 answer</h1>
<p>Request %SOZU_ID could not be parsed.</p>
<pre>
Kawa error: %DETAILS
<pre>
"[..],
    )
}

fn default_401() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 401 Unauthorized\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %SOZU_ID\r
\r\n"[..],
    )
}

fn default_404() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 404 Not Found\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %SOZU_ID\r
\r\n"[..],
    )
}

fn default_408() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 408 Request Timeout\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %SOZU_ID\r
\r\n"[..],
    )
}

fn default_413() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 413 Payload Too Large\r
Cache-Control: no-cache\r
Connection: close\r
%Content-Length: %CONTENT_LENGTH\r
Sozu-Id: %SOZU_ID\r
\r
<h1>Sozu automatic 413 answer</h1>
<p>Request %SOZU_ID failed because all its headers could not be contained at once</p>
<pre>
Kawa cursors: %DETAILS
<pre>
"[..],
    )
}

fn default_502() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 502 Bad Gateway\r
Cache-Control: no-cache\r
Connection: close\r
%Content-Length: %CONTENT_LENGTH\r
Sozu-Id: %SOZU_ID\r
\r
<h1>Sozu automatic 502 answer</h1>
<p>Response to %SOZU_ID could not be parsed.</p>
<pre>
Kawa error: %DETAILS
<pre>
"[..],
    )
}

fn default_503() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 503 Service Unavailable\r
Cache-Control: no-cache\r
Connection: close\r
%Content-Length: %CONTENT_LENGTH\r
Sozu-Id: %SOZU_ID\r
\r
<h1>Sozu automatic 503 answer</h1>
<p>Could not find an available backend for request %SOZU_ID.</p>
<pre>
%DETAILS
<pre>
"[..],
    )
}

fn default_504() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 504 Gateway Timeout\r
Cache-Control: no-cache\r
Connection: close\r
Sozu-Id: %SOZU_ID\r
\r\n"[..],
    )
}

fn default_507() -> Vec<u8> {
    Vec::from(
        &b"\
HTTP/1.1 507 Insufficient Storage\r
Cache-Control: no-cache\r
Connection: close\r
%Content-Length: %CONTENT_LENGTH\r
Sozu-Id: %SOZU_ID\r
\r
<h1>Sozu automatic 507 answer</h1>
<p>Response to %SOZU_ID failed because all its headers could not be contained at once.</p>
<pre>
Kawa cursors: %DETAILS
<pre>
"[..],
    )
}

impl HttpAnswers {
    pub fn new(conf: &CustomHttpAnswers) -> Result<Self, (u16, TemplateError)> {
        let sozu_id = TemplateVariable {
            name: "SOZU_ID",
            valid_in_body: true,
            valid_in_header: true,
            typ: ReplacementType::Variable(0),
        };
        let length = TemplateVariable {
            name: "CONTENT_LENGTH",
            valid_in_body: false,
            valid_in_header: true,
            typ: ReplacementType::ContentLength,
        };
        let details = TemplateVariable {
            name: "DETAILS",
            valid_in_body: true,
            valid_in_header: false,
            typ: ReplacementType::VariableOnce(0),
        };
        let location = TemplateVariable {
            name: "REDIRECT_LOCATION",
            valid_in_body: false,
            valid_in_header: true,
            typ: ReplacementType::VariableOnce(0),
        };
        let ans_301: Vec<u8> = conf
            .answer_301
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_301());
        let answer_400: Vec<u8> = conf
            .answer_400
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_400());
        let answer_401: Vec<u8> = conf
            .answer_401
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_401());
        let answer_404: Vec<u8> = conf
            .answer_404
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_404());
        let answer_408: Vec<u8> = conf
            .answer_408
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_408());
        let answer_413: Vec<u8> = conf
            .answer_413
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_413());
        let answer_502: Vec<u8> = conf
            .answer_502
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_502());
        let answer_503: Vec<u8> = conf
            .answer_503
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_503());
        let answer_504: Vec<u8> = conf
            .answer_504
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_504());
        let answer_507: Vec<u8> = conf
            .answer_507
            .clone()
            .map(|a| a.into_bytes())
            .unwrap_or(default_507());

        Ok(HttpAnswers {
            default: DefaultAnswers {
                answer_301: Template::new(301, ans_301, &[sozu_id, length, location])?,
                answer_400: Template::new(400, answer_400, &[sozu_id, length, details])?,
                answer_401: Template::new(401, answer_401, &[sozu_id, length])?,
                answer_404: Template::new(404, answer_404, &[sozu_id, length])?,
                answer_408: Template::new(408, answer_408, &[sozu_id, length])?,
                answer_413: Template::new(413, answer_413, &[sozu_id, length, details])?,
                answer_502: Template::new(502, answer_502, &[sozu_id, length, details])?,
                answer_503: Template::new(503, answer_503, &[sozu_id, length, details])?,
                answer_504: Template::new(504, answer_504, &[sozu_id, length])?,
                answer_507: Template::new(507, answer_507, &[sozu_id, length, details])?,
            },
            custom: HashMap::new(),
        })
    }

    // TODO: rename to add_503_answer
    pub fn add_custom_answer(
        &mut self,
        cluster_id: &str,
        answer_503: String,
    ) -> Result<(), (u16, TemplateError)> {
        let answer_503 = Template::new(503, answer_503.into(), &[])?;
        self.custom
            .insert(cluster_id.to_string(), CustomAnswers { answer_503 });
        Ok(())
    }

    pub fn remove_custom_answer(&mut self, cluster_id: &str) {
        self.custom.remove(cluster_id);
    }

    pub fn get(
        &self,
        answer: DefaultAnswer,
        cluster_id: Option<&str>,
        request_id: String,
    ) -> DefaultAnswerStream {
        let mut variables_once: Vec<Vec<u8>>;
        let template = match answer {
            DefaultAnswer::Answer301 { location } => {
                variables_once = vec![location.into()];
                &self.default.answer_301
            }
            DefaultAnswer::Answer400 { details } => {
                variables_once = vec![details.into()];
                &self.default.answer_400
            }
            DefaultAnswer::Answer401 {} => {
                variables_once = vec![];
                &self.default.answer_401
            }
            DefaultAnswer::Answer404 {} => {
                variables_once = vec![];
                &self.default.answer_404
            }
            DefaultAnswer::Answer408 {} => {
                variables_once = vec![];
                &self.default.answer_408
            }
            DefaultAnswer::Answer413 { details } => {
                variables_once = vec![details.into()];
                &self.default.answer_413
            }
            DefaultAnswer::Answer502 { details } => {
                variables_once = vec![details.into()];
                &self.default.answer_502
            }
            DefaultAnswer::Answer503 { details } => {
                variables_once = vec![details.into()];
                cluster_id
                    .and_then(|id: &str| self.custom.get(id))
                    .map(|c| &c.answer_503)
                    .unwrap_or_else(|| &self.default.answer_503)
            }
            DefaultAnswer::Answer504 {} => {
                variables_once = vec![];
                &self.default.answer_504
            }
            DefaultAnswer::Answer507 { details } => {
                variables_once = vec![details.into()];
                &self.default.answer_507
            }
        };
        // kawa::debug_kawa(&template.kawa);
        // println!("{template:#?}");
        template.fill(&[request_id.as_bytes()], &mut variables_once)
    }
}
