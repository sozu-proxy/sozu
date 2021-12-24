use nom;

use nom::{
    bytes::complete::is_not,
    // bytes::streaming::is_not,
    // character::streaming::char,
    combinator::{complete, cut, map_res},
    error::Error as NomError,
    error::{ErrorKind, FromExternalError},
    multi::many0,
    sequence::terminated,
    IResult,
};
use serde_json::from_str;

#[derive(Debug)]
pub struct CustomError {
    kind: ErrorKind,
    serde_json_error: Option<serde_json::Error>,
}

impl FromExternalError<&str, serde_json::Error> for CustomError {
    fn from_external_error(
        input: &str,
        kind: ErrorKind,
        serde_json_error: serde_json::Error,
    ) -> Self {
        println!("input: {}, error kind: {:?}", input, kind);
        Self {
            kind,
            serde_json_error: Some(serde_json_error),
        }
    }
}

impl nom::error::ParseError<&str> for CustomError {
    fn from_error_kind(input: &str, kind: ErrorKind) -> Self {
        println!("input: {}, error kind: {:?}", input, kind);

        Self {
            kind,
            serde_json_error: None,
        }
    }

    fn append(_input: &str, _kind: ErrorKind, other: Self) -> Self {
        other
    }
}

pub fn parse_one_struct<'de, T>(input: &str) -> IResult<&str, T, CustomError>
where
    T: serde::de::Deserialize<'de>,
{
    let (next_input, json_data) = is_not("\n")(input)?;

    let parsed_struct = match serde_json::from_str::<T>(json_data) {
        Ok(user) => user,
        Err(serde_error) => {
            return Err(nom::Err::Failure(CustomError::from_external_error(
                input,
                ErrorKind::MapRes,
                serde_error,
            )))
        }
    };

    let (next_input, _) = nom::character::complete::char('\n')(i)?;

    Ok((next_input, parsed_struct))
}

pub fn parse_several_structs<'de, T>(input: &str) -> IResult<&str, Vec<T>, CustomError>
where
    T: serde::de::Deserialize<'de>,
{
    many0(parse_one_struct)(input)
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::command::{CommandRequest, CommandRequestData};

    #[test]
    fn parse_one_command_request_works() {
        let command_request = CommandRequest::new(
            "Some request".to_string(),
            CommandRequestData::DumpState,
            Some(5),
        );

        let mut stringified_request = serde_json::ser::to_string(&command_request).unwrap();
        stringified_request.push('\0');

        assert_eq!(
            parse_several_structs::<CommandRequest>(&stringified_request).unwrap(),
            ("", command_request)
        )
    }

    #[test]
    fn parse_several_command_requestes_works() {
        let vector_of_requests = vec![
            CommandRequest::new(
                "Some request".to_string(),
                CommandRequestData::SaveState {
                    path: "/some/path".to_string(),
                },
                Some(5),
            ),
            CommandRequest::new(
                "Some other request".to_string(),
                CommandRequestData::SubscribeEvents,
                Some(4),
            ),
            CommandRequest::new(
                "Yet another request".to_string(),
                CommandRequestData::DumpState,
                None,
            ),
        ];

        let mut stringified_requests: String;

        // append command requests and \0 chars



        println!("{}", stringified_users);
        assert_eq!(
            parse_several_users(&stringified_users).unwrap(),
            ("", random_users)
        )
    }

    #[test]
    fn bad_input_yields_an_error() {
        let bad_users_input = r#"{"username":345,"password":"hV9StRA"}
{"username":"qETqU6t","password":"gykzW8x"}
{"username":"2vhA0B0","password":"SDGJDGk"}
"#;

        assert!(
            parse_several_users(bad_users_input).is_err(),
            // Err(nom::error::Error(serde_json::Error { err: "bla" }))
            // nom::Err::Error(nom::error::Error { input: bad_users_input, code: ErrorKind::Fail })
        );
    }
}
