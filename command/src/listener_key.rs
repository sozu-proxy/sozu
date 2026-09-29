//! Listener identity: a socket address, plus the network interface the
//! listening socket is bound to (`SO_BINDTODEVICE`) when there is one.
//!
//! Two listeners may share an address when they are bound to different
//! interfaces (`0.0.0.0:443` on `wg0` and `0.0.0.0:443` on `eth1`), so the
//! address alone no longer names a listener. Every table that stores or
//! hands over listeners — `ConfigState`'s listener maps, `ListenersList`, the
//! `ListenersCount` manifest and `Listeners` of the SCM hand-off — is keyed by
//! a [`ListenerKey`](crate::listener_key::ListenerKey) instead.
//!
//! The key has a text form, used as a map key in JSON (`ConfigState` crosses
//! a main-process upgrade serialized as JSON) and in the protobuf string maps
//! and lists: the socket address as `SocketAddr` prints it, followed by
//! `%<interface>` when the listener is bound to one. A listener without an
//! interface keeps exactly the text it had before interfaces existed, so
//! state serialized by an older Sōzu still loads, and one written by this one
//! loads in an older Sōzu as long as no listener names an interface.

use std::{fmt, net::SocketAddr, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::proto::command::{
    ActivateListener, DeactivateListener, HttpListenerConfig, HttpsListenerConfig, RemoveListener,
    SocketAddress, TcpListenerConfig, UdpListenerConfig,
};

/// Separates the address from the interface in the text form of a key.
///
/// A `SocketAddr` prints `%` only inside the brackets of an IPv6 address with
/// a scope id (`[fe80::1%2]:80`), and [`validate_interface_name`] refuses `%`
/// in an interface name, so the last `%` of a key that does not parse as a
/// bare address is always the separator.
pub const INTERFACE_SEPARATOR: char = '%';

/// Longest interface name the kernel accepts: `IFNAMSIZ` (16) minus the NUL.
pub const MAX_INTERFACE_NAME_LEN: usize = 15;

/// Why an interface name was refused.
#[derive(thiserror::Error, Debug, PartialEq, Eq, Clone)]
pub enum InterfaceError {
    #[error("the listener interface name is empty")]
    Empty,
    #[error(
        "the listener interface name '{0}' is longer than {MAX_INTERFACE_NAME_LEN} bytes (IFNAMSIZ)"
    )]
    TooLong(String),
    #[error(
        "the listener interface name '{0}' is not a valid interface name: it must not be '.' or \
         '..' nor contain '/', ':', '%', whitespace or NUL"
    )]
    InvalidCharacter(String),
    #[error(
        "the listener interface '{0}' is not supported on this platform: binding a listener to a \
         network interface uses SO_BINDTODEVICE, which only Linux provides"
    )]
    Unsupported(String),
}

/// Check `name` against the kernel's own rules for an interface name
/// (`dev_valid_name` in Linux `net/core/dev.c`), plus `%`, which the text
/// form of a [`ListenerKey`] uses as a separator.
pub fn validate_interface_name(name: &str) -> Result<(), InterfaceError> {
    if name.is_empty() {
        return Err(InterfaceError::Empty);
    }
    if name.len() > MAX_INTERFACE_NAME_LEN {
        return Err(InterfaceError::TooLong(name.to_owned()));
    }
    if name == "."
        || name == ".."
        || name.chars().any(|c| {
            c == '/' || c == ':' || c == INTERFACE_SEPARATOR || c == '\0' || c.is_whitespace()
        })
    {
        return Err(InterfaceError::InvalidCharacter(name.to_owned()));
    }
    debug_assert!(
        !name.contains(INTERFACE_SEPARATOR),
        "an accepted interface name must never contain the key separator"
    );
    Ok(())
}

/// Validate a listener `interface` for the platform Sōzu runs on.
///
/// Binding to an interface uses `SO_BINDTODEVICE`, which exists only on
/// Linux: anywhere else the key is refused instead of silently listening on
/// every interface.
pub fn validate_listener_interface(name: &str) -> Result<(), InterfaceError> {
    validate_listener_interface_on(name, cfg!(target_os = "linux"))
}

/// [`validate_listener_interface`] with the platform verdict as a parameter,
/// so the refusal path is exercised by the tests of every platform.
fn validate_listener_interface_on(
    name: &str,
    platform_has_bind_to_device: bool,
) -> Result<(), InterfaceError> {
    validate_interface_name(name)?;
    if !platform_has_bind_to_device {
        return Err(InterfaceError::Unsupported(name.to_owned()));
    }
    Ok(())
}

/// Why the text form of a listener key could not be parsed.
#[derive(thiserror::Error, Debug, PartialEq, Eq, Clone)]
pub enum ListenerKeyError {
    #[error("invalid listener address '{0}'")]
    Address(String),
    #[error("invalid listener key '{key}': {error}")]
    Interface { key: String, error: InterfaceError },
}

/// The identity of a listener: its address and, when it is bound to one, its
/// network interface. `interface: None` is a listener as Sōzu had them before
/// interfaces existed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListenerKey {
    pub address: SocketAddr,
    pub interface: Option<String>,
}

impl ListenerKey {
    pub fn new(address: impl Into<SocketAddr>, interface: Option<&str>) -> Self {
        Self {
            address: address.into(),
            interface: interface.map(ToOwned::to_owned),
        }
    }

    /// Whether this key names the listener at `address` on `interface`.
    pub fn matches(&self, address: &SocketAddr, interface: Option<&str>) -> bool {
        self.address == *address && self.interface.as_deref() == interface
    }
}

impl From<SocketAddr> for ListenerKey {
    fn from(address: SocketAddr) -> Self {
        Self {
            address,
            interface: None,
        }
    }
}

impl From<SocketAddress> for ListenerKey {
    fn from(address: SocketAddress) -> Self {
        SocketAddr::from(address).into()
    }
}

impl fmt::Display for ListenerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.interface {
            None => write!(f, "{}", self.address),
            Some(interface) => write!(f, "{}{INTERFACE_SEPARATOR}{interface}", self.address),
        }
    }
}

impl FromStr for ListenerKey {
    type Err = ListenerKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(address) = s.parse::<SocketAddr>() {
            return Ok(address.into());
        }
        let Some((address, interface)) = s.rsplit_once(INTERFACE_SEPARATOR) else {
            return Err(ListenerKeyError::Address(s.to_owned()));
        };
        let address = address
            .parse::<SocketAddr>()
            .map_err(|_| ListenerKeyError::Address(s.to_owned()))?;
        validate_interface_name(interface).map_err(|error| ListenerKeyError::Interface {
            key: s.to_owned(),
            error,
        })?;
        Ok(Self::new(address, Some(interface)))
    }
}

impl Serialize for ListenerKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ListenerKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

macro_rules! impl_listener_key {
    ($($message:ty),+ $(,)?) => {
        $(
            impl $message {
                /// The identity of the listener this message configures or targets.
                pub fn listener_key(&self) -> ListenerKey {
                    ListenerKey::new(self.address, self.interface.as_deref())
                }
            }
        )+
    };
}

impl_listener_key!(
    HttpListenerConfig,
    HttpsListenerConfig,
    TcpListenerConfig,
    UdpListenerConfig,
    ActivateListener,
    DeactivateListener,
    RemoveListener,
);

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn text_form_round_trips_with_and_without_interface() {
        for (text, address, interface) in [
            ("0.0.0.0:443", "0.0.0.0:443", None),
            ("0.0.0.0:443%wg0", "0.0.0.0:443", Some("wg0")),
            ("[::]:8080%eth0.100", "[::]:8080", Some("eth0.100")),
            ("[fe80::1%2]:80", "[fe80::1%2]:80", None),
            ("[fe80::1%2]:80%wg0", "[fe80::1%2]:80", Some("wg0")),
        ] {
            let key: ListenerKey = text.parse().expect(text);
            assert_eq!(key.address, address.parse::<SocketAddr>().unwrap());
            assert_eq!(key.interface.as_deref(), interface);
            assert_eq!(key.to_string(), text);
        }
    }

    /// A key without an interface prints and serializes exactly as the bare
    /// `SocketAddr` did, which is what keeps a state serialized before this
    /// type existed loadable, and one written now loadable by an older Sōzu.
    #[test]
    fn a_key_without_interface_serializes_as_the_bare_address() {
        let address: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let legacy: BTreeMap<SocketAddr, u8> = [(address, 1)].into();
        let legacy_json = serde_json::to_string(&legacy).unwrap();

        let keyed: BTreeMap<ListenerKey, u8> = [(ListenerKey::from(address), 1)].into();
        assert_eq!(serde_json::to_string(&keyed).unwrap(), legacy_json);
        assert_eq!(
            serde_json::from_str::<BTreeMap<ListenerKey, u8>>(&legacy_json).unwrap(),
            keyed
        );

        let with_interface: BTreeMap<ListenerKey, u8> =
            [(ListenerKey::new(address, Some("wg0")), 1)].into();
        let json = serde_json::to_string(&with_interface).unwrap();
        assert_eq!(json, r#"{"127.0.0.1:8080%wg0":1}"#);
        assert_eq!(
            serde_json::from_str::<BTreeMap<ListenerKey, u8>>(&json).unwrap(),
            with_interface
        );
    }

    #[test]
    fn malformed_keys_are_refused() {
        assert!(matches!(
            "not-an-address".parse::<ListenerKey>(),
            Err(ListenerKeyError::Address(_))
        ));
        assert!(matches!(
            "nope%wg0".parse::<ListenerKey>(),
            Err(ListenerKeyError::Address(_))
        ));
        assert!(matches!(
            "0.0.0.0:80%".parse::<ListenerKey>(),
            Err(ListenerKeyError::Interface {
                error: InterfaceError::Empty,
                ..
            })
        ));
    }

    #[test]
    fn interface_names_follow_the_kernel_rules() {
        for valid in [
            "wg0",
            "eth0.100",
            "enp3s0f1",
            "a",
            "x234567890123456".get(..MAX_INTERFACE_NAME_LEN).unwrap(),
        ] {
            assert_eq!(validate_interface_name(valid), Ok(()), "{valid}");
        }
        assert_eq!(validate_interface_name(""), Err(InterfaceError::Empty));
        assert!(matches!(
            validate_interface_name("x2345678901234567"),
            Err(InterfaceError::TooLong(_))
        ));
        for invalid in [
            ".", "..", "wg/0", "wg:0", "wg%0", "wg 0", "wg\t0", "wg\x000",
        ] {
            assert!(
                matches!(
                    validate_interface_name(invalid),
                    Err(InterfaceError::InvalidCharacter(_))
                ),
                "{invalid:?}"
            );
        }
    }

    /// Without `SO_BINDTODEVICE` the key is refused, never ignored.
    #[test]
    fn listener_interface_is_refused_without_bind_to_device() {
        let error = validate_listener_interface_on("wg0", false).unwrap_err();
        assert_eq!(error, InterfaceError::Unsupported("wg0".to_owned()));
        assert!(error.to_string().contains("only Linux provides"));
        // An invalid name is reported as such first, on every platform.
        assert_eq!(
            validate_listener_interface_on("", false),
            Err(InterfaceError::Empty)
        );
        assert_eq!(validate_listener_interface_on("wg0", true), Ok(()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn listener_interface_is_accepted_on_linux() {
        assert_eq!(validate_listener_interface("wg0"), Ok(()));
    }

    /// The platform verdict itself: off Linux the public validator refuses.
    /// Compiled only for a non-Linux target.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn listener_interface_is_refused_off_linux() {
        assert_eq!(
            validate_listener_interface("wg0"),
            Err(InterfaceError::Unsupported("wg0".to_owned()))
        );
    }
}
