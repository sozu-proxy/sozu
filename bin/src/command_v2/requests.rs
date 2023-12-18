use std::{collections::BTreeMap, fs::File};

use mio::Token;
use sozu_command_lib::{
    config::Config,
    proto::command::{
        request::RequestType, response_content::ContentType, FrontendFilters, QueryClustersHashes,
        Request, ResponseContent, ResponseStatus, WorkerResponses,
    },
    response::WorkerResponse,
};

use crate::command_v2::{ClientSession, DefaultGatherer, Gatherer, GatheringTask, Server};

//===============================================
// List frontends

pub fn list_frontend_command(
    server: &mut Server,
    client: &mut ClientSession,
    filters: FrontendFilters,
) {
    let response = server
        .query_main(&RequestType::ListFrontends(filters).into())
        .unwrap();
    client.finish_ok(response);
}

//===============================================
// Query clusters

#[derive(Debug)]
pub struct QueryClustersCommand {
    pub client_token: Token,
    pub request_type: RequestType,
    pub gatherer: DefaultGatherer,
    main_process_response: Option<ResponseContent>,
}

pub fn query_clusters(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    let task = Box::new(QueryClustersCommand {
        client_token: client.token,
        request_type: request_content.clone(),
        gatherer: DefaultGatherer::default(),
        main_process_response: server.query_main(&request_content).unwrap(),
    });
    client.return_processing("Querying cluster hashes...");

    server.scatter(request_content.into(), task)
}

impl GatheringTask for QueryClustersCommand {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(&mut self, server: &mut Server, client: &mut ClientSession) {
        let mut worker_responses: BTreeMap<String, ResponseContent> = self
            .gatherer
            .responses
            .drain(..)
            .filter_map(|(worker_id, proxy_response)| {
                proxy_response
                    .content
                    .map(|response_content| (worker_id.to_string(), response_content))
            })
            .collect();
        if let Some(main_response) = &self.main_process_response {
            worker_responses.insert(String::from("main"), main_response.clone());
        }

        client.finish_ok(Some(
            ContentType::WorkerResponses(WorkerResponses {
                map: worker_responses,
            })
            .into(),
        ));
    }
}

//===============================================
// Query clusters

#[derive(Debug)]
pub struct QueryClustersByIdCommand {
    pub client_token: Token,
    pub data: QueryClustersHashes,
    pub gatherer: DefaultGatherer,
}

pub fn query_cluster_hashes(
    server: &mut Server,
    client: &mut ClientSession,
    data: QueryClustersHashes,
) {
    let task = Box::new(QueryClustersByIdCommand {
        client_token: client.token,
        data: data.clone(),
        gatherer: DefaultGatherer::default(),
    });
    client.return_processing("Querying cluster hashes...");
    server.scatter(RequestType::QueryClustersHashes(data).into(), task)
}

impl GatheringTask for QueryClustersByIdCommand {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(&mut self, server: &mut Server, client: &mut ClientSession) {
        client.finish_ok(None);
    }
}

//===============================================
// Load static configuration

#[derive(Debug)]
struct LoadStaticConfig {
    pub ok: usize,
    pub error: usize,
    pub expected_responses: usize,
}

pub fn load_static_config(server: &mut Server) {
    let callback = Box::new(LoadStaticConfig {
        ok: 0,
        error: 0,
        expected_responses: 0,
    });
    let task_id = server.new_task(callback);
    for (request_index, message) in server
        .config
        .generate_config_messages()
        .unwrap()
        .into_iter()
        .enumerate()
    {
        let request = message.content;
        if let Err(e) = server.state.dispatch(&request) {
            error!("Could not execute request on state: {:#}", e);
        }

        if let &Some(RequestType::AddCertificate(_)) = &request.request_type {
            debug!("config generated AddCertificate( ... )");
        } else {
            debug!("config generated {:?}", request);
        }

        server.scatter_on(request, task_id, request_index);
    }
}

impl GatheringTask for LoadStaticConfig {
    fn client_token(&self) -> Option<Token> {
        None
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        self
    }

    fn on_finish_no_client(&mut self, server: &mut Server) {
        if self.error == 0 {
            info!("loading state: {} ok messages, 0 errors", self.ok);
        } else {
            error!(
                "loading state: {} ok messages, {} errors",
                self.ok, self.error
            );
        }
        server.backends_count = server.state.count_backends();
        server.frontends_count = server.state.count_frontends();
        gauge!("configuration.clusters", server.state.clusters.len());
        gauge!("configuration.backends", server.backends_count);
        gauge!("configuration.frontends", server.frontends_count);
    }
}

impl Gatherer for LoadStaticConfig {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn on_message_no_client(
        &mut self,
        server: &mut Server,
        _worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        match message.status {
            ResponseStatus::Ok => {
                self.ok += 1;
            }
            ResponseStatus::Processing => {
                info!("processing");
            }
            ResponseStatus::Failure => {
                error!(
                    "error handling configuration message {}: {}",
                    message.id, message.message
                );
                self.error += 1;
            }
        }
        self.ok + self.error >= self.expected_responses
    }
}

// =========================================================
// Worker request

#[derive(Debug)]
struct WorkerRequest {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
}

pub fn worker_request(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    let request: Request = request_content.into();

    if let Err(e) = server.state.dispatch(&request) {
        return client.finish_failure(&format!(
            "could not dispatch request on the main process state: {e}",
        ));
    }

    server.scatter(
        request,
        Box::new(WorkerRequest {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
        }),
    )
}

impl GatheringTask for WorkerRequest {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(&mut self, _server: &mut Server, client: &mut ClientSession) {
        let mut messages = vec![];
        let mut has_error = false;

        for (worker_id, response) in self.gatherer.responses.drain(..) {
            match response.status {
                ResponseStatus::Failure => {
                    messages.push(format!("{}: {}", worker_id, response.message));
                    has_error = true;
                }
                _ => messages.push(format!("{}: OK", worker_id)),
            }
        }

        if has_error {
            client.finish_failure(messages.join(", "));
        } else {
            client.finish_ok(None);
        }
    }
}
