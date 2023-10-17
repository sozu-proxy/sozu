use prost::DecodeError;

/// Contains all types received by and sent from Sōzu
pub mod command;

/// Implementation of fmt::Display for the protobuf types, used in the CLI
pub mod display;

#[derive(thiserror::Error, Debug)]
pub enum DisplayError {
    #[error("Could not display content")]
    DisplayContent(String),
    #[error("Error while parsing response to JSON")]
    Json(serde_json::Error),
    #[error("got the wrong response content type: {0}")]
    WrongResponseType(String),
    #[error("Could not format the datetime to ISO 8601")]
    DateTime,
    #[error("unrecognized protobuf variant: {0}")]
    DecodeError(DecodeError),
}

// Simple helper to build ResponseContent from ContentType
impl From<command::response_content::ContentType> for command::ResponseContent {
    fn from(value: command::response_content::ContentType) -> Self {
        Self {
            content_type: Some(value),
        }
    }
}

// Simple helper to build Request from RequestType
impl From<command::request::RequestType> for command::Request {
    fn from(value: command::request::RequestType) -> Self {
        Self {
            request_type: Some(value),
        }
    }
}
