use sozu_command_lib::proto::command::{
    request::RequestType, FrontendFilters, QueryClustersHashes,
};

use crate::command_v2::{ClientSession, GatheredResponses, GatheringCallback, Server};

#[derive(Debug)]
pub struct QueryClustersHashesCommand {
    pub data: QueryClustersHashes,
}
pub fn query_cluster_hashes(
    server: &mut Server,
    client: &mut ClientSession,
    data: QueryClustersHashes,
) {
    let callback = Box::new(QueryClustersHashesCommand { data: data.clone() });
    server.scatter(
        "Querying cluster hashes...",
        client,
        RequestType::QueryClustersHashes(data).into(),
        callback,
    )
}
impl GatheringCallback for QueryClustersHashesCommand {
    fn gather(
        &mut self,
        server: &mut Server,
        client: &mut ClientSession,
        responses: GatheredResponses,
    ) {
        println!("{responses:#?}");
        client.finish_ok(None);
    }
}

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
