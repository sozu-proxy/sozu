// NOTE: I'll open a PR on procinfo-rs to parse /proc/sys/fs/file-max
// in the meantime, I wrote it here
// Arnaud Lefebvre - 23/02/17

use std::fs::File;
use std::io::{Read, Error, ErrorKind};

static FILE_MAX_PATH: &'static str = "/proc/sys/fs/file-max";

// kernel uses get_max_files which returns a unsigned long
// see include/linux/fs.h
pub fn limits_file_max() -> Result<usize, Error> {
    let mut buf = String::new();
    let mut file = try!(File::open(FILE_MAX_PATH));
    try!(file.read_to_string(&mut buf));
    buf.trim().parse::<usize>().or(Err(Error::new(ErrorKind::InvalidInput, format!("Couldn't parse {} value into usize", FILE_MAX_PATH))))
}
