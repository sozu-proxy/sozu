use serde::{Deserialize, Serialize};

/// keeps track of a request metadata, as handled by the CommandServer
#[derive(Clone, Debug, Serialize)]
pub struct RequestSummary {
    /// the worker we send the request to
    pub worker_id: u32,
    /// the request id as sent within ProxyRequest
    pub id: String,
    /// the client who sent the request
    pub client: Option<String>,
    /// In certain cases, the same response may need to be transmitted several times over
    pub expected_responses: usize,
}

impl RequestSummary {
    pub fn default_with_request_id(id: &str) -> Self {
        Self {
            worker_id: 0, // defaults to zero, don't forget to update!
            id: id.to_owned(),
            client: None,
            expected_responses: 1,
        }
    }

    pub fn with_worker(&self, worker_id: &u32) -> Self {
        Self {
            id: self.id,
            worker_id: worker_id.to_owned(),
            client: self.client,
            expected_responses: self.expected_responses,
        }
    }

    pub fn with_client(self, client_id: &str) -> Self {
        self.client = Some(client_id.to_owned());
        self
    }

    pub fn with_expected_responses(&mut self, expected_responses: usize) {
        self.expected_responses = expected_responses;
    }

    pub fn append_suffix_to_id(&self, suffix: &str) -> Self {
        let mut new_request_id = String::new();
        match self.client {
            Some(client) => new_request_id.push_str(&format!("{}-", &client)),
            None => {}
        }
        new_request_id.push_str(&format!("{}-", suffix));
        new_request_id.push_str(&format!("{}-", self.id));
        match self.client {
            Some(worker) => new_request_id.push_str(&format!("{}", &worker)),
            None => {}
        }
        Self {
            id: new_request_id,
            worker_id: self.worker_id,
            client: self.client,
            expected_responses: self.expected_responses,
        }
    }

    pub fn add_counter(&self, counter: usize) -> Self {
        Self {
            id: format!("{}-{}", self.id, counter),
            worker_id: self.worker_id,
            client: self.client,
            expected_responses: self.expected_responses,
        }
    }

    pub fn id(&self) -> String {
        self.id.to_owned()
    }
}
