//! Service identity: normalised host class, port and optional service id.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use super::{Match, Signal};
use crate::model::ServiceMode;

/// Normalised class of a service host. No DNS resolution is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostClass {
    /// `localhost`, `127.0.0.0/8` or `::1`.
    Loopback,
    /// `0.0.0.0`, `::` or the empty string: overlaps every host.
    Wildcard,
    /// Any other literal IP address.
    Ip(IpAddr),
    /// A host name, lower-cased.
    Name(String),
}

impl HostClass {
    /// Classifies a host string.
    #[must_use]
    pub fn parse(host: &str) -> Self {
        let host = host.trim();
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            return Self::Wildcard;
        }
        if host.eq_ignore_ascii_case("localhost") {
            return Self::Loopback;
        }
        match host.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => Self::Wildcard,
            Ok(ip) if ip.is_loopback() => Self::Loopback,
            Ok(ip) => Self::Ip(ip),
            Err(_) => Self::Name(host.to_lowercase()),
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        matches!(self, Self::Wildcard) || matches!(other, Self::Wildcard) || self == other
    }
}

/// Identity of a network service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceIdentity {
    /// Connect or listen.
    pub mode: ServiceMode,
    /// URL scheme, if any.
    pub scheme: Option<String>,
    /// Normalised host.
    pub host_class: HostClass,
    /// TCP port.
    pub port: u16,
    /// Service-specific identifier, if any.
    pub service_id: Option<String>,
}

impl ServiceIdentity {
    /// Builds the identity from service settings fields.
    #[must_use]
    pub fn from_settings(
        mode: ServiceMode,
        scheme: Option<&str>,
        host: &str,
        port: u16,
        service_id: Option<&str>,
    ) -> Self {
        Self {
            mode,
            scheme: scheme.map(str::to_owned),
            host_class: HostClass::parse(host),
            port,
            service_id: service_id.map(str::to_owned),
        }
    }
}

/// A `service_id` present and equal on both sides is a hit on its own, even
/// with a different host or port. Otherwise the endpoints match when the ports
/// are equal and the hosts overlap. Mode and scheme are not compared.
pub(super) fn matches(a: &ServiceIdentity, b: &ServiceIdentity) -> Match {
    if let (Some(x), Some(y)) = (&a.service_id, &b.service_id)
        && x == y
    {
        return Match::Hit(Signal::ServiceId);
    }
    if a.port == b.port && a.host_class.overlaps(&b.host_class) {
        return Match::Hit(Signal::Endpoint);
    }
    Match::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_classes() {
        for h in [
            "localhost",
            "LocalHost",
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "[::1]",
        ] {
            assert_eq!(HostClass::parse(h), HostClass::Loopback, "{h}");
        }
        for h in ["0.0.0.0", "::", "", "[::]"] {
            assert_eq!(HostClass::parse(h), HostClass::Wildcard, "{h:?}");
        }
        assert_eq!(
            HostClass::parse("10.0.0.5"),
            HostClass::Ip("10.0.0.5".parse().unwrap())
        );
        assert_eq!(
            HostClass::parse("Db.Example.COM"),
            HostClass::Name("db.example.com".into())
        );
    }
}
