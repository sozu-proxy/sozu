/// Contains all types received by and sent from Sōzu
pub mod command;

/// Implementation of fmt::Display for the protobuf types, used in the CLI
pub mod display;

// Simple helper to build ResponseContent from ContentType
impl From<command::response_content::ContentType> for command::ResponseContent {
    fn from(value: command::response_content::ContentType) -> Self {
        Self {
            content_type: Some(value),
        }
    }
}
