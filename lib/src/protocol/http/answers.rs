use std::rc::Rc;
use std::collections::HashMap;
use AppId;
use super::DefaultAnswerStatus;

#[allow(non_snake_case)]
pub struct DefaultAnswers {
  /// 400
  pub BadRequest:         Rc<Vec<u8>>,
  /// 401
  pub Unauthorized:       Rc<Vec<u8>>,
  /// 404
  pub NotFound:           Rc<Vec<u8>>,
  /// 408
  pub RequestTimeout:     Rc<Vec<u8>>,
  /// 413
  pub PayloadTooLarge:    Rc<Vec<u8>>,
  /// 503
  pub ServiceUnavailable: Rc<Vec<u8>>,
  /// 504
  pub GatewayTimeout:     Rc<Vec<u8>>,
}

#[allow(non_snake_case)]
pub struct CustomAnswers {
  pub ServiceUnavailable: Option<Rc<Vec<u8>>>,
}

pub struct HttpAnswers {
  pub default: DefaultAnswers,
  pub custom:  HashMap<AppId, CustomAnswers>,
}

impl HttpAnswers {
  pub fn new(answer_404: &str, answer_503: &str) -> Self {
    HttpAnswers {
      default: DefaultAnswers {
        BadRequest: Rc::new(Vec::from(
          &b"HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
        )),
        Unauthorized: Rc::new(Vec::from(
          &b"HTTP/1.1 401 Unauthorized\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
        )),
        NotFound: Rc::new(Vec::from(answer_404.as_bytes())),
        RequestTimeout: Rc::new(Vec::from(
          &b"HTTP/1.1 408 Request Timeout\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
        )),
        PayloadTooLarge: Rc::new(Vec::from(
          &b"HTTP/1.1 413 Payload Too Large\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
        )),
        ServiceUnavailable: Rc::new(Vec::from(answer_503.as_bytes())),
        GatewayTimeout: Rc::new(Vec::from(
          &b"HTTP/1.1 504 Gateway Timeout\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
        )),
      },
      custom: HashMap::new(),
    }
  }

  pub fn add_custom_answer(&mut self, app_id: &str, answer_503: &str) {
    let a = answer_503.to_owned();
    self.custom
      .entry(app_id.to_string())
      .and_modify(|c| c.ServiceUnavailable = Some(Rc::new(a.into_bytes())))
      .or_insert(CustomAnswers{ ServiceUnavailable: Some(Rc::new(answer_503.to_owned().into_bytes())) });
  }

  pub fn remove_custom_answer(&mut self, app_id: &str) {
    self.custom.remove(app_id);
  }

  pub fn get(&self, answer: DefaultAnswerStatus, app_id: Option<&str>) -> Rc<Vec<u8>> {
    match answer {
      DefaultAnswerStatus::Answer301 => panic!("the 301 answer is generated dynamically"),
      DefaultAnswerStatus::Answer400 => self.default.BadRequest.clone(),
      DefaultAnswerStatus::Answer401 => self.default.Unauthorized.clone(),
      DefaultAnswerStatus::Answer404 => self.default.NotFound.clone(),
      DefaultAnswerStatus::Answer408 => self.default.RequestTimeout.clone(),
      DefaultAnswerStatus::Answer413 => self.default.PayloadTooLarge.clone(),
      DefaultAnswerStatus::Answer503 => app_id.and_then(|id: &str| self.custom.get(id))
        .and_then(|c| c.ServiceUnavailable.clone()).unwrap_or_else(|| self.default.ServiceUnavailable.clone()),
      DefaultAnswerStatus::Answer504 => self.default.GatewayTimeout.clone(),

    }

  }

}
