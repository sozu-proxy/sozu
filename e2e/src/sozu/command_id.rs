/// Utility struct to generate unique command ids by auto increment
/// and keep track of the last id used
pub struct CommandID {
    pub id: usize,
    pub prefix: String,
    pub last: String,
}

impl CommandID {
    pub fn new() -> Self {
        Self {
            id: 0,
            prefix: "ID_".to_owned(),
            last: "NONE".to_owned(),
        }
    }

    pub fn next(&mut self) -> String {
        let id = format!("{}{}", self.prefix, self.id);
        id.clone_into(&mut self.last);
        self.id += 1;
        id
    }
}
