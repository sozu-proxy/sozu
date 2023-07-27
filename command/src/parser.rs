use nom::{
    self,
    error::{ErrorKind, FromExternalError},
    multi::many0,
    IResult,
};

#[derive(Debug)]
#[allow(dead_code)]
pub struct ParseError {
    kind: ErrorKind,
    serde_json_error: Option<serde_json::Error>,
}

impl FromExternalError<&[u8], serde_json::Error> for ParseError {
    fn from_external_error(
        _input: &[u8],
        kind: ErrorKind,
        serde_json_error: serde_json::Error,
    ) -> Self {
        // println!("input: {:?}, error kind: {:?}", input, kind);
        Self {
            kind,
            serde_json_error: Some(serde_json_error),
        }
    }
}

impl nom::error::ParseError<&[u8]> for ParseError {
    fn from_error_kind(_input: &[u8], kind: ErrorKind) -> Self {
        // println!("input: {:?}, error kind: {:?}", input, kind);

        Self {
            kind,
            serde_json_error: None,
        }
    }

    fn append(_input: &[u8], _kind: ErrorKind, other: Self) -> Self {
        other
    }
}

/// Parse a single Request or WorkerRequest
pub fn parse_one_request<'a, T>(input: &'a [u8]) -> IResult<&[u8], T, ParseError>
where
    T: serde::de::Deserialize<'a>,
{
    let (next_input, json_data) = nom::bytes::complete::is_not("\0")(input)?;

    let command = match serde_json::from_slice::<T>(json_data) {
        Ok(user) => user,
        Err(serde_error) => {
            return Err(nom::Err::Failure(ParseError::from_external_error(
                input,
                ErrorKind::MapRes,
                serde_error,
            )))
        }
    };

    let (next_input, _) = nom::character::complete::char('\0')(next_input)?;

    Ok((next_input, command))
}

/// Parse a Requests or WorkerRequests using nom
pub fn parse_several_requests<'a, T>(input: &'a [u8]) -> IResult<&[u8], Vec<T>, ParseError>
where
    T: serde::de::Deserialize<'a>,
{
    many0(parse_one_request)(input)
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::{
        proto::command::{request::RequestType, Status, SubscribeEvents},
        request::WorkerRequest,
    };

    #[test]
    fn parse_one_worker_request() {
        let worker_request = WorkerRequest::new(
            "Some request".to_string(),
            RequestType::Status(Status {}).into(),
        );

        let mut string = serde_json::ser::to_string(&worker_request).unwrap();

        string.push('\0');

        println!("string to parse: {string}");

        let bytes = &string.as_bytes();

        let empty_vec: Vec<u8> = vec![];

        assert_eq!(
            parse_one_request(bytes).unwrap(),
            (&empty_vec[..], worker_request)
        )
    }

    #[test]
    fn parse_several_worker_requests() {
        let requests = vec![
            WorkerRequest::new(
                "Some request".to_string(),
                RequestType::SaveState("/some/path".to_string()).into(),
            ),
            WorkerRequest::new(
                "Some other request".to_string(),
                RequestType::SubscribeEvents(SubscribeEvents {}).into(),
            ),
            WorkerRequest::new(
                "Yet another request".to_string(),
                RequestType::Status(Status {}).into(),
            ),
        ];

        let mut serialized_requests = String::new();

        for request in requests.iter() {
            serialized_requests += &serde_json::ser::to_string(&request).unwrap();
            serialized_requests.push('\0');
        }

        let bytes_to_parse = &serialized_requests.as_bytes();

        let parsed_requests = parse_several_requests(bytes_to_parse).unwrap();

        println!("parsed commands: {parsed_requests:?}");

        let empty_vec: Vec<u8> = vec![];

        assert_eq!(parsed_requests, (&empty_vec[..], requests))
    }
}
