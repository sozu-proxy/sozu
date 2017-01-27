use std::fmt::Debug;

#[macro_export]
macro_rules! unwrap_msg (
  ($e:expr) => (
    ($e).unwrap_log(file!(), line!(), module_path!(), stringify!($e))
  )
);

pub trait UnwrapLog<T> {
   fn unwrap_log(self, file: &str, line: u32, module_path: &str, expression: &str) -> T;
}

impl<T> UnwrapLog<T> for Option<T> {
  fn unwrap_log(self, file: &str, line: u32, module_path: &str, expression: &str) -> T {
    match self {
      Some(T) => T,
      None    => panic!("{}:{} {} this should not have panicked:\ntried to unwrap `None` in:\nunwrap_msg!({})", file, line, module_path, expression)
    }
  }
}

impl<T,E:Debug> UnwrapLog<T> for Result<T,E> {
  fn unwrap_log(self, file: &str, line: u32, module_path: &str, expression: &str) -> T {
    match self {
      Ok(T)  => T,
      Err(e) => panic!("{}:{} {} this should not have panicked:\ntried to unwrap Err({:?}) in\nunwrap_msg!({})", file, line, module_path, e, expression)
    }
  }
}
