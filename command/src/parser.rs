use nom::{
    self,
    error::{ErrorKind, FromExternalError},
    multi::many0,
    IResult,
};

#[derive(Debug)]
#[allow(dead_code)]
pub struct CustomError {
    kind: ErrorKind,
    serde_json_error: Option<serde_json::Error>,
}

impl FromExternalError<&[u8], serde_json::Error> for CustomError {
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

impl nom::error::ParseError<&[u8]> for CustomError {
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

pub fn parse_one_command<'a, T>(input: &'a [u8]) -> IResult<&[u8], T, CustomError>
where
    T: serde::de::Deserialize<'a>,
{
    let (next_input, json_data) = nom::bytes::complete::is_not("\0")(input)?;

    let command = match serde_json::from_slice::<T>(json_data) {
        Ok(user) => user,
        Err(serde_error) => {
            return Err(nom::Err::Failure(CustomError::from_external_error(
                input,
                ErrorKind::MapRes,
                serde_error,
            )))
        }
    };

    let (next_input, _) = nom::character::complete::char('\0')(next_input)?;

    Ok((next_input, command))
}

pub fn parse_several_commands<'a, T>(input: &'a [u8]) -> IResult<&[u8], Vec<T>, CustomError>
where
    T: serde::de::Deserialize<'a>,
{
    many0(parse_one_command)(input)
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::request::{Request, WorkerRequest};

    #[test]
    fn parse_one_inner_order_works() {
        let inner_order = WorkerRequest::new("Some request".to_string(), Request::DumpState);

        let mut string = serde_json::ser::to_string(&inner_order).unwrap();

        string.push('\0');

        println!("string to parse: {string}");

        let bytes = &string.as_bytes();

        let empty_vec: Vec<u8> = vec![];

        assert_eq!(
            parse_one_command(bytes).unwrap(),
            (&empty_vec[..], inner_order)
        )
    }

    #[test]
    fn parse_several_inner_orders_works() {
        let orders = vec![
            WorkerRequest::new(
                "Some request".to_string(),
                Request::SaveState {
                    path: "/some/path".to_string(),
                },
            ),
            WorkerRequest::new("Some other request".to_string(), Request::SubscribeEvents),
            WorkerRequest::new("Yet another request".to_string(), Request::DumpState),
        ];

        let mut serialized_orders = String::new();

        for order in orders.iter() {
            serialized_orders += &serde_json::ser::to_string(&order).unwrap();
            serialized_orders.push('\0');
        }

        let bytes_to_parse = &serialized_orders.as_bytes();

        let parsed_orders = parse_several_commands(bytes_to_parse).unwrap();

        println!("parsed commands: {parsed_orders:?}");

        let empty_vec: Vec<u8> = vec![];

        assert_eq!(parsed_orders, (&empty_vec[..], orders))
    }
}
