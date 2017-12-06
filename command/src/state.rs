use std::collections::{BTreeMap,HashMap,HashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use std::iter::FromIterator;
use certificate::calculate_fingerprint;

use messages::{Application,CertFingerprint,CertificateAndKey,Order,
  HttpFront,HttpsFront,TcpFront,Instance,QueryAnswerApplication};

pub type AppId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct HttpProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<AppId, Vec<HttpFront>>,
  instances:  HashMap<AppId, Vec<Instance>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize, Deserialize)]
pub struct HttpsProxy {
  ip_address:   String,
  port:         u16,
  certificates: HashMap<CertFingerprint, CertificateAndKey>,
  fronts:       HashMap<AppId, Vec<HttpsFront>>,
  instances:    HashMap<AppId, Vec<Instance>>,
}

#[derive(Debug,Default,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct ConfigState {
  pub applications:    HashMap<AppId, Application>,
  pub instances:       HashMap<AppId, Vec<Instance>>,
  pub http_fronts:     HashMap<AppId, Vec<HttpFront>>,
  pub https_fronts:    HashMap<AppId, Vec<HttpsFront>>,
  pub tcp_fronts:      HashMap<AppId, Vec<TcpFront>>,
  pub certificates:    HashMap<CertFingerprint, CertificateAndKey>,
  //ip, port
  pub http_addresses:  Vec<(String, u16)>,
  pub https_addresses: Vec<(String, u16)>,
  //tcp:
}

impl ConfigState {
  pub fn new() -> ConfigState {
    ConfigState {
      applications:    HashMap::new(),
      instances:       HashMap::new(),
      http_fronts:     HashMap::new(),
      https_fronts:    HashMap::new(),
      tcp_fronts:      HashMap::new(),
      certificates:    HashMap::new(),
      http_addresses:  Vec::new(),
      https_addresses: Vec::new(),
    }
  }

  pub fn add_http_address(&mut self, ip_address: String, port: u16) {
    self.http_addresses.push((ip_address, port))
  }

  pub fn add_https_address(&mut self, ip_address: String, port: u16) {
    self.https_addresses.push((ip_address, port))
  }

  pub fn handle_order(&mut self, order: &Order) {
    match order {
      &Order::AddApplication(ref application) => {
        let app = application.clone();
        self.applications.insert(app.app_id.clone(), app);
      },
      &Order::RemoveApplication(ref app_id) => {
        self.applications.remove(app_id);
      },
      &Order::AddHttpFront(ref front) => {
        let front_vec = self.http_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
        }
      },
      &Order::RemoveHttpFront(ref front) => {
        if let Some(front_list) = self.http_fronts.get_mut(&front.app_id) {
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
        }
      },
      &Order::AddCertificate(ref certificate_and_key) => {
        let fingerprint = match calculate_fingerprint(&certificate_and_key.certificate.as_bytes()[..]) {
          Some(f)  => CertFingerprint(f),
          None => {
            error!("cannot obtain the certificate's fingerprint");
            return;
          }
        };

        if !self.certificates.contains_key(&fingerprint) {
          self.certificates.insert(fingerprint.clone(), certificate_and_key.clone());
        }
      },
      &Order::RemoveCertificate(ref fingerprint) => {
        self.certificates.remove(fingerprint);
      },
      &Order::AddHttpsFront(ref front) => {
        let front_vec = self.https_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
        }
      },
      &Order::RemoveHttpsFront(ref front) => {
        if let Some(front_list) = self.https_fronts.get_mut(&front.app_id) {
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
        }
      },
      &Order::AddTcpFront(ref front) => {
        let front_vec = self.tcp_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
        }
      },
      &Order::RemoveTcpFront(ref front) => {
        if let Some(front_list) = self.tcp_fronts.get_mut(&front.app_id) {
          front_list.retain(|el| el.ip_address != front.ip_address || el.port != front.port);
        }
      },
      &Order::AddInstance(ref instance)  => {
        let instance_vec = self.instances.entry(instance.app_id.clone()).or_insert(vec!());
        if !instance_vec.contains(&instance) {
          instance_vec.push(instance.clone());
        }
      },
      &Order::RemoveInstance(ref instance) => {
        if let Some(instance_list) = self.instances.get_mut(&instance.app_id) {
          instance_list.retain(|el| el.ip_address != instance.ip_address || el.port != instance.port);
        }
      },
      // This is to avoid the error message
      &Order::Logging(_) | &Order::Status => {},
      o => {
        error!("state cannot handle order message: {:#?}", o);
      }
    }
  }

  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    for app in self.applications.values() {
      v.push(Order::AddApplication(app.clone()));
    }

    for front_list in self.http_fronts.values() {
      for front in front_list {
        v.push(Order::AddHttpFront(front.clone()));
      }
    }

    for certificate_and_key in self.certificates.values() {
      v.push(Order::AddCertificate(certificate_and_key.clone()));
    }

    for front_list in self.https_fronts.values() {
      for front in front_list {
        v.push(Order::AddHttpsFront(front.clone()));
      }
    }

    for front_list in self.tcp_fronts.values() {
      for front in front_list {
        v.push(Order::AddTcpFront(front.clone()));
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
    let my_apps: HashSet<&AppId>    = self.applications.keys().collect();
    let their_apps: HashSet<&AppId> = other.applications.keys().collect();

    let removed_apps = my_apps.difference(&their_apps);
    let added_apps: Vec<&Application> = their_apps.difference(&my_apps).filter_map(|app_id| other.applications.get(app_id.as_str())).collect();

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

    let mut my_fronts: HashSet<(&AppId, &HttpsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.https_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &HttpsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.https_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_https_fronts = my_fronts.difference(&their_fronts);
    let added_https_fronts   = their_fronts.difference(&my_fronts);

    let mut my_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_tcp_fronts = my_fronts.difference(&their_fronts);
    let added_tcp_fronts   = their_fronts.difference(&my_fronts);

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

    for app_id in removed_apps {
      v.push(Order::RemoveApplication(app_id.to_string()));
    }

    for app in added_apps {
      v.push(Order::AddApplication(app.clone()));
    }

    for &(_, certificate_and_key) in added_certificates {
      v.push(Order::AddCertificate(certificate_and_key.clone()));
    }

    for &(_, front) in removed_http_fronts {
     v.push(Order::RemoveHttpFront(front.clone()));
    }

    for &(_, front) in removed_https_fronts {
     v.push(Order::RemoveHttpsFront(front.clone()));
    }

    for &(_, front) in removed_tcp_fronts {
     v.push(Order::RemoveTcpFront(front.clone()));
    }

    for &(_, instance) in added_instances {
      v.push(Order::AddInstance(instance.clone()));
    }

    for &(_, instance) in removed_instances {
      v.push(Order::RemoveInstance(instance.clone()));
    }

    for &(_, front) in added_http_fronts {
      v.push(Order::AddHttpFront(front.clone()));
    }

    for &(_, front) in added_https_fronts {
      v.push(Order::AddHttpsFront(front.clone()));
    }

    for &(_, front) in added_tcp_fronts {
      v.push(Order::AddTcpFront(front.clone()));
    }

    for  &(fingerprint, _) in removed_certificates {
      v.push(Order::RemoveCertificate(fingerprint.clone()));
    }
    v
  }

  pub fn hash_state(&self) -> BTreeMap<AppId, u64> {
    self.instances.keys().map(|app_id| {
      let mut s = DefaultHasher::new();
      self.applications.get(app_id).hash(&mut s);
      self.instances.get(app_id).hash(&mut s);
      self.http_fronts.get(app_id).hash(&mut s);
      self.https_fronts.get(app_id).hash(&mut s);
      self.tcp_fronts.get(app_id).hash(&mut s);

      (app_id.to_string(), s.finish())

    }).collect()
  }

  pub fn application_state(&self, app_id: &str) -> QueryAnswerApplication {
    QueryAnswerApplication {
      configuration:   self.applications.get(app_id).cloned(),
      http_frontends:  self.http_fronts.get(app_id).cloned().unwrap_or(vec!()),
      https_frontends: self.https_fronts.get(app_id).cloned().unwrap_or(vec!()),
      tcp_frontends:   self.tcp_fronts.get(app_id).cloned().unwrap_or(vec!()),
      backends:        self.instances.get(app_id).cloned().unwrap_or(vec!()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use messages::{Order,HttpFront,Instance};

  #[test]
  fn serialize() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_2"), instance_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-3"),ip_address: String::from("192.168.1.3"), port: 1027 }));
    state.handle_order(&Order::RemoveInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-3"), ip_address: String::from("192.168.1.3"), port: 1027 }));

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
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_2"), instance_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026 }));
    state.handle_order(&Order::AddApplication(Application { app_id: String::from("app_2"), sticky_session: true }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026 }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027 }));
    state2.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028 }));
    state2.handle_order(&Order::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false }));

   let e = vec!(
     Order::RemoveHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }),
     Order::RemoveInstance(Instance { app_id: String::from("app_2"), instance_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026 }),
     Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028 }),
     Order::RemoveApplication(String::from("app_2")),
     Order::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false }),
   );
   let expected_diff:HashSet<&Order> = HashSet::from_iter(e.iter());

   let d = state.diff(&state2);
   let diff = HashSet::from_iter(d.iter());
   println!("diff orders:\n{:#?}\n", diff);
   println!("expected diff orders:\n{:#?}\n", expected_diff);

   let hash1 = state.hash_state();
   let hash2 = state2.hash_state();
   let mut state3 = state.clone();
   state3.handle_order(&Order::AddInstance(Instance { app_id: String::from("app_1"), instance_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028 }));
   let hash3 = state3.hash_state();
   println!("state 1 hashes: {:#?}", hash1);
   println!("state 2 hashes: {:#?}", hash2);
   println!("state 3 hashes: {:#?}", hash3);

   assert_eq!(diff, expected_diff);
  }
}
