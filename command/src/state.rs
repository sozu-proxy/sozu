use std::collections::{HashMap,HashSet};

use std::iter::FromIterator;
use openssl::x509::X509;
use openssl::hash::MessageDigest;

use sozu::messages::{CertFingerprint,CertificateAndKey,Order,HttpFront,TlsFront,Instance};

pub type AppId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct HttpProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<AppId, Vec<HttpFront>>,
  instances:  HashMap<AppId, Vec<Instance>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize, Deserialize)]
pub struct TlsProxy {
  ip_address:   String,
  port:         u16,
  certificates: HashMap<CertFingerprint, CertificateAndKey>,
  fronts:       HashMap<AppId, Vec<TlsFront>>,
  instances:    HashMap<AppId, Vec<Instance>>,
}

#[derive(Debug,Default,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct ConfigState {
  instances: HashMap<AppId, Vec<Instance>>,
  http_fronts: HashMap<AppId, Vec<HttpFront>>,
  https_fronts: HashMap<AppId, Vec<TlsFront>>,
  certificates: HashMap<CertFingerprint, CertificateAndKey>,
  //ip, port
  http_addresses: Vec<(String, u16)>,
  https_addresses: Vec<(String, u16)>,
  //tcp:
}

impl ConfigState {
  pub fn add_http_address(&mut self, ip_address: String, port: u16) {
    self.http_addresses.push((ip_address, port))
  }

  pub fn add_https_address(&mut self, ip_address: String, port: u16) {
    self.https_addresses.push((ip_address, port))
  }

  pub fn handle_order(&mut self, order: &Order) {
    match order {
      &Order::AddHttpFront(ref front) => {
        let f = HttpFront {
          app_id:     front.app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
        };
        if self.http_fronts.contains_key(&front.app_id) {
          self.http_fronts.get_mut(&front.app_id).map(|front| {
            if !front.contains(&f) {
              front.push(f);
            }
          });
        } else {
          self.http_fronts.insert(front.app_id.clone(), vec!(f));
        }
      },
      &Order::RemoveHttpFront(ref front) => {
        if let Some(front_list) = self.http_fronts.get_mut(&front.app_id) {
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
        }
      },
      &Order::AddCertificate(ref certificate_and_key) => {
        let f = CertificateAndKey {
          certificate:       certificate_and_key.certificate.clone(),
          certificate_chain: certificate_and_key.certificate_chain.clone(),
          key:               certificate_and_key.key.clone(),
        };
        let fingerprint = match X509::from_pem(&certificate_and_key.certificate.as_bytes()[..]).and_then(|cert|
                                  cert.fingerprint(MessageDigest::sha256())) {
          Ok(f)  => f,
          Err(e) => {
            error!("cannot obtain the certificate's fingerprint: {:?}", e);
            return;
          }
        };

        if !self.certificates.contains_key(&fingerprint) {
          self.certificates.insert(fingerprint.clone(), f);
        }
      },
      &Order::RemoveCertificate(ref fingerprint) => {
        self.certificates.remove(fingerprint);
      },
      &Order::AddTlsFront(ref front) => {
        let f = TlsFront {
          app_id:      front.app_id.clone(),
          hostname:    front.hostname.clone(),
          path_begin:  front.path_begin.clone(),
          fingerprint: front.fingerprint.clone(),
        };
        if self.https_fronts.contains_key(&front.app_id) {
          self.https_fronts.get_mut(&front.app_id).map(|front| {
            if !front.contains(&f) {
              front.push(f);
            }
          });
        } else {
          self.https_fronts.insert(front.app_id.clone(), vec!(f));
        }
      },
      &Order::RemoveTlsFront(ref front) => {
        self.https_fronts.remove(&front.app_id);
      },
      &Order::AddInstance(ref instance)  => {
        if self.instances.contains_key(&instance.app_id) {
          self.instances.get_mut(&instance.app_id).map(|instance_vec| {
            if !instance_vec.contains(&instance) {
              instance_vec.push(instance.clone());
            }
          });
        } else {
          self.instances.insert(instance.app_id.clone(), vec!(instance.clone()));
        }
      },
      &Order::RemoveInstance(ref instance) => {
        if let Some(instance_list) = self.instances.get_mut(&instance.app_id) {
          instance_list.retain(|el| el.ip_address != instance.ip_address || el.port != instance.port);
        }
      }
      _ => {}
    }
  }

  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();
    for (app_id, front_list) in self.http_fronts.iter() {
      for front in front_list {
        v.push(Order::AddHttpFront(HttpFront {
          app_id:     app_id.clone(),
          hostname:   front.hostname.clone(),
          path_begin: front.path_begin.clone(),
        }));
      }
    }

    let mut v = Vec::new();
    for certificate_and_key in (&self.certificates).values() {
      v.push(Order::AddCertificate(certificate_and_key.clone()));
    }

    for (app_id, front_list) in &self.https_fronts {
      for front in front_list {
        v.push(Order::AddTlsFront(TlsFront {
          app_id:      app_id.clone(),
          hostname:    front.hostname.clone(),
          path_begin:  front.path_begin.clone(),
          fingerprint: front.fingerprint.clone(),
        }));
      }
    }

    for instance_list in self.instances.values() {
      for instance in instance_list {
        v.push(Order::AddInstance(instance.clone()));
      }
    }

    v
  }

  pub fn diff(&self, other:&ConfigState) -> Vec<Order> {
    let mut my_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.http_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.http_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_http_fronts = my_fronts.difference(&their_fronts);
    let added_http_fronts   = their_fronts.difference(&my_fronts);

    let mut my_fronts: HashSet<(&AppId, &TlsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.https_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &TlsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.https_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_https_fronts = my_fronts.difference(&their_fronts);
    let added_https_fronts   = their_fronts.difference(&my_fronts);

    let mut my_instances: HashSet<(&AppId, &Instance)> = HashSet::new();
    for (ref app_id, ref instance_list) in self.instances.iter() {
      for ref instance in instance_list.iter() {
        my_instances.insert((&app_id, &instance));
      }
    }
    let mut their_instances: HashSet<(&AppId, &Instance)> = HashSet::new();
    for (ref app_id, ref instance_list) in other.instances.iter() {
      for ref instance in instance_list.iter() {
        their_instances.insert((&app_id, &instance));
      }
    }

    let removed_instances = my_instances.difference(&their_instances);
    let added_instances   = their_instances.difference(&my_instances);

    let my_certificates:    HashSet<(&CertFingerprint, &CertificateAndKey)> = HashSet::from_iter(self.certificates.iter());
    let their_certificates: HashSet<(&CertFingerprint, &CertificateAndKey)> = HashSet::from_iter(other.certificates.iter());

    let removed_certificates = my_certificates.difference(&their_certificates);
    let added_certificates   = their_certificates.difference(&my_certificates);

    let mut v = vec!();

    for &(_, certificate_and_key) in added_certificates {
      v.push(Order::AddCertificate(certificate_and_key.clone()));
    }

    for &(app_id, front) in removed_http_fronts {
     v.push(Order::RemoveHttpFront(HttpFront {
       app_id:     app_id.clone(),
       hostname:   front.hostname.clone(),
       path_begin: front.path_begin.clone(),
      }));
    }

    for &(app_id, front) in removed_https_fronts {
     v.push(Order::RemoveTlsFront(TlsFront {
       app_id:      app_id.clone(),
       hostname:    front.hostname.clone(),
       path_begin:  front.path_begin.clone(),
       fingerprint: front.fingerprint.clone(),
      }));
    }

    for &(_, instance) in added_instances {
      v.push(Order::AddInstance(instance.clone()));
    }

    for &(_, instance) in removed_instances {
      v.push(Order::RemoveInstance(instance.clone()));
    }

    for &(app_id, front) in added_http_fronts {
      v.push(Order::AddHttpFront(HttpFront {
        app_id:     app_id.clone(),
        hostname:   front.hostname.clone(),
        path_begin: front.path_begin.clone(),
      }));
    }

    for &(app_id, front) in added_https_fronts {
      v.push(Order::AddTlsFront(TlsFront {
        app_id:      app_id.clone(),
        hostname:    front.hostname.clone(),
        path_begin:  front.path_begin.clone(),
        fingerprint: front.fingerprint.clone(),
      }));
    }

    for  &(fingerprint, _) in removed_certificates {
      v.push(Order::RemoveCertificate(fingerprint.clone()));
    }
    v
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use sozu::messages::{Order,HttpFront,Instance};

  #[test]
  fn serialize() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_2"), ip_address: String::from("192.167.1.2"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("192.168.1.3"), port: 1027 }));
    state.handle_order(&Order::RemoveInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("192.168.1.3"), port: 1027 }));

    /*
    let encoded = state.encode();
    println!("serialized:\n{}", encoded);

    let new_state: Option<HttpProxy> = decode_str(&encoded);
    println!("deserialized:\n{:?}", new_state);
    assert_eq!(new_state, Some(state));
    */
    //assert!(false);
  }

  #[test]
  fn diff() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_2"), ip_address: String::from("192.167.1.2"), port: 1026 }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1028 }));

   let e = vec!(
     Order::RemoveHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }),
     Order::RemoveInstance(Instance { app_id: String::from("app_2"), ip_address: String::from("192.167.1.2"), port: 1026 }),
     Order::AddInstance(Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.2"), port: 1028 }),
   );
   let expected_diff:HashSet<&Order> = HashSet::from_iter(e.iter());

   let d = state.diff(&state2);
   let diff = HashSet::from_iter(d.iter());
   println!("diff orders:\n{:?}\n", diff);
   println!("expected diff orders:\n{:?}\n", expected_diff);

   assert_eq!(diff, expected_diff);
  }
}
