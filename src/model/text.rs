//! Resource and unit descriptions, and usage templates with unit placeholders.
//!
//! Pure text rules: nothing here touches the store or the clock.

use serde_json::{Value, json};
use thiserror::Error;

use crate::error::ErrorCode;
use crate::model::ids::{ResourceId, UnitId};
use crate::model::types::{ResourceType, ServiceMode, UnitSettings};

/// Longest description, in Unicode scalar values.
pub const DESCRIPTION_MAX_CHARS: usize = 200;
/// Longest usage template, in UTF-8 bytes.
pub const USAGE_MAX_BYTES: usize = 4096;

/// Why a description or usage template was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextError {
    /// The description exceeds [`DESCRIPTION_MAX_CHARS`].
    #[error("description is longer than {max} characters")]
    DescriptionTooLong {
        /// The limit.
        max: usize,
    },
    /// The description contains a line break.
    #[error("description must be a single line")]
    DescriptionMultiline,
    /// The usage template exceeds [`USAGE_MAX_BYTES`].
    #[error("usage is longer than {max} bytes")]
    UsageTooLong {
        /// The limit.
        max: usize,
    },
    /// A placeholder is not a valid key for the resource type.
    #[error("unknown placeholder {{{placeholder}}}; valid: {}", .valid.join(", "))]
    UnknownPlaceholder {
        /// The key between the braces.
        placeholder: String,
        /// The sorted valid keys for the type.
        valid: Vec<String>,
    },
}

impl ErrorCode for TextError {
    fn code(&self) -> &'static str {
        match self {
            TextError::DescriptionTooLong { .. } => "description_too_long",
            TextError::DescriptionMultiline => "description_multiline",
            TextError::UsageTooLong { .. } => "usage_too_long",
            TextError::UnknownPlaceholder { .. } => "unknown_placeholder",
        }
    }

    fn details(&self) -> Option<Value> {
        match self {
            TextError::UnknownPlaceholder { placeholder, valid } => {
                Some(json!({ "placeholder": placeholder, "valid": valid }))
            }
            _ => None,
        }
    }
}

const DEVICE_KEYS: &[&str] = &[
    "resource.description",
    "resource.id",
    "unit.description",
    "unit.id",
    "unit.path",
];

const SERVICE_KEYS: &[&str] = &[
    "resource.description",
    "resource.id",
    "unit.address",
    "unit.description",
    "unit.host",
    "unit.id",
    "unit.mode",
    "unit.port",
    "unit.scheme",
    "unit.service_id",
];

const GENERIC_KEYS: &[&str] = &[
    "resource.description",
    "resource.id",
    "unit.description",
    "unit.id",
    "unit.identity_key",
];

/// The placeholder keys (without braces) valid for `ty`, sorted.
#[must_use]
pub fn placeholder_keys(ty: ResourceType) -> &'static [&'static str] {
    match ty {
        ResourceType::Device => DEVICE_KEYS,
        ResourceType::Service => SERVICE_KEYS,
        ResourceType::Generic => GENERIC_KEYS,
    }
}

/// A one-line description of a resource or unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Description(String);

/// A Markdown usage template with `{resource.*}` / `{unit.*}` placeholders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTemplate(String);

/// The resource facts a template can mention.
#[derive(Debug, Clone, Copy)]
pub struct RenderResource<'a> {
    /// The resource id.
    pub id: &'a ResourceId,
    /// The resource description.
    pub description: &'a str,
}

/// The unit facts a template can mention.
#[derive(Debug, Clone, Copy)]
pub struct RenderUnit<'a> {
    /// The unit id.
    pub id: &'a UnitId,
    /// The unit description.
    pub description: &'a str,
    /// The unit settings.
    pub settings: &'a UnitSettings,
}

impl Description {
    /// Validate `text` as a description. It is stored as given, untrimmed.
    ///
    /// # Errors
    /// [`TextError::DescriptionMultiline`] on `\n` or `\r`;
    /// [`TextError::DescriptionTooLong`] over [`DESCRIPTION_MAX_CHARS`] characters.
    pub fn parse(text: &str) -> Result<Self, TextError> {
        if text.contains(['\n', '\r']) {
            return Err(TextError::DescriptionMultiline);
        }
        if text.chars().count() > DESCRIPTION_MAX_CHARS {
            return Err(TextError::DescriptionTooLong {
                max: DESCRIPTION_MAX_CHARS,
            });
        }
        Ok(Self(text.to_owned()))
    }

    /// The description as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One piece of a template.
enum Segment<'a> {
    /// Text copied through unchanged.
    Literal(&'a str),
    /// A well-formed placeholder; the key is without braces.
    Placeholder(&'a str),
}

/// Length of the placeholder key at the start of `rest` (the text after a
/// `{`) if it is `(resource|unit).[a-z0-9_]+` followed by `}`.
fn placeholder_len(rest: &str) -> Option<usize> {
    let after_ns = rest
        .strip_prefix("resource.")
        .or_else(|| rest.strip_prefix("unit."))?;
    let ns_len = rest.len() - after_ns.len();
    let key_len = after_ns
        .bytes()
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        .count();
    (key_len > 0 && after_ns.as_bytes().get(key_len) == Some(&b'}')).then_some(ns_len + key_len)
}

/// Split `text` into literals and placeholders, in one pass. Shared by
/// [`UsageTemplate::parse`] and [`UsageTemplate::render`].
fn scan(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut literal_start = 0;
    let mut i = 0;
    while i < text.len() {
        // `{` is ASCII, so byte positions of it are always char boundaries.
        if text.as_bytes()[i] == b'{'
            && let Some(len) = placeholder_len(&text[i + 1..])
        {
            if literal_start < i {
                segments.push(Segment::Literal(&text[literal_start..i]));
            }
            segments.push(Segment::Placeholder(&text[i + 1..i + 1 + len]));
            i += len + 2;
            literal_start = i;
            continue;
        }
        i += 1;
    }
    if literal_start < text.len() {
        segments.push(Segment::Literal(&text[literal_start..]));
    }
    segments
}

/// `host:port`, with an IPv6 host (containing `:`) bracketed.
fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The value of `key` for this resource and unit; `None` when the key does
/// not exist for the unit's settings. Mirrors `UnitSettings::env_vars`.
fn value(key: &str, resource: &RenderResource<'_>, unit: &RenderUnit<'_>) -> Option<String> {
    let text = match (key, unit.settings) {
        ("resource.id", _) => resource.id.as_str().to_owned(),
        ("resource.description", _) => resource.description.to_owned(),
        ("unit.id", _) => unit.id.as_str().to_owned(),
        ("unit.description", _) => unit.description.to_owned(),
        ("unit.path", UnitSettings::Device { path }) => path.display().to_string(),
        ("unit.mode", UnitSettings::Service { mode, .. }) => match mode {
            ServiceMode::Connect => "connect",
            ServiceMode::Listen => "listen",
        }
        .to_owned(),
        ("unit.scheme", UnitSettings::Service { scheme, .. }) => scheme.clone().unwrap_or_default(),
        ("unit.host", UnitSettings::Service { host, .. }) => host.clone(),
        ("unit.port", UnitSettings::Service { port, .. }) => port.to_string(),
        ("unit.service_id", UnitSettings::Service { service_id, .. }) => {
            service_id.clone().unwrap_or_default()
        }
        ("unit.address", UnitSettings::Service { host, port, .. }) => address(host, *port),
        ("unit.identity_key", UnitSettings::Generic { identity_key }) => identity_key.clone(),
        _ => return None,
    };
    Some(text)
}

impl UsageTemplate {
    /// Validate `text` as a usage template for a resource of type `ty`.
    ///
    /// # Errors
    /// [`TextError::UsageTooLong`] over [`USAGE_MAX_BYTES`] bytes;
    /// [`TextError::UnknownPlaceholder`] for the first placeholder whose key
    /// is not in [`placeholder_keys`] for `ty`.
    pub fn parse(text: &str, ty: ResourceType) -> Result<Self, TextError> {
        if text.len() > USAGE_MAX_BYTES {
            return Err(TextError::UsageTooLong {
                max: USAGE_MAX_BYTES,
            });
        }
        let valid = placeholder_keys(ty);
        for segment in scan(text) {
            if let Segment::Placeholder(key) = segment
                && !valid.contains(&key)
            {
                return Err(TextError::UnknownPlaceholder {
                    placeholder: key.to_owned(),
                    valid: valid.iter().map(|k| (*k).to_owned()).collect(),
                });
            }
        }
        Ok(Self(text.to_owned()))
    }

    /// Wrap text read back from storage.
    ///
    /// Only for text that passed [`UsageTemplate::parse`] when stored; no
    /// check is repeated here.
    #[must_use]
    pub(crate) fn from_stored(text: String) -> Self {
        Self(text)
    }

    /// The raw template text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Fill in the placeholders for one unit. Single pass: inserted values are
    /// never scanned again. A placeholder with no value for this unit (only
    /// possible if the template was not parsed for its type) stays as written.
    #[must_use]
    pub fn render(&self, resource: &RenderResource<'_>, unit: &RenderUnit<'_>) -> String {
        let mut out = String::with_capacity(self.0.len());
        for segment in scan(&self.0) {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Placeholder(key) => {
                    if let Some(v) = value(key, resource, unit) {
                        out.push_str(&v);
                    } else {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn rid() -> ResourceId {
        ResourceId::parse("rig").unwrap()
    }

    fn uid() -> UnitId {
        UnitId::parse("u1").unwrap()
    }

    fn device() -> UnitSettings {
        UnitSettings::Device {
            path: PathBuf::from("/dev/ttyUSB0"),
        }
    }

    fn service(host: &str, port: u16) -> UnitSettings {
        UnitSettings::Service {
            mode: ServiceMode::Connect,
            scheme: None,
            host: host.to_owned(),
            port,
            service_id: None,
        }
    }

    fn render_with(template: &str, ty: ResourceType, settings: &UnitSettings) -> String {
        let t = UsageTemplate::parse(template, ty).unwrap();
        let (r, u) = (rid(), uid());
        t.render(
            &RenderResource {
                id: &r,
                description: "the rig",
            },
            &RenderUnit {
                id: &u,
                description: "unit one",
                settings,
            },
        )
    }

    mod description_parse {
        use super::*;

        mod on_empty_text {
            use super::*;

            #[test]
            fn returns_an_empty_description() {
                assert_eq!(Description::parse("").unwrap().as_str(), "");
                assert_eq!(Description::default().as_str(), "");
            }
        }

        mod on_200_characters_multibyte_included {
            use super::*;

            #[test]
            fn returns_the_description_unchanged() {
                let text = format!("  {}é", "世".repeat(197));
                assert_eq!(text.chars().count(), 200);
                assert_eq!(Description::parse(&text).unwrap().as_str(), text);
            }
        }

        mod on_201_characters {
            use super::*;

            #[test]
            fn returns_description_too_long() {
                let text = "a".repeat(201);
                assert_eq!(
                    Description::parse(&text),
                    Err(TextError::DescriptionTooLong { max: 200 })
                );
            }
        }

        mod on_text_containing_newline_or_carriage_return {
            use super::*;

            #[test]
            fn returns_description_multiline() {
                for text in ["a\nb", "a\rb", "\n"] {
                    assert_eq!(
                        Description::parse(text),
                        Err(TextError::DescriptionMultiline)
                    );
                }
            }
        }
    }

    mod usage_template_parse {
        use super::*;

        mod on_empty_text {
            use super::*;

            #[test]
            fn returns_an_empty_template() {
                let t = UsageTemplate::parse("", ResourceType::Device).unwrap();
                assert_eq!(t.as_str(), "");
                assert_eq!(UsageTemplate::default().as_str(), "");
            }
        }

        mod on_text_with_valid_placeholders_for_the_type {
            use super::*;

            #[test]
            fn returns_the_template_with_raw_text_unchanged() {
                let text = "Use {unit.path} on {resource.id} ({unit.id}, {resource.description}, {unit.description})";
                let t = UsageTemplate::parse(text, ResourceType::Device).unwrap();
                assert_eq!(t.as_str(), text);
            }
        }

        mod on_4096_bytes {
            use super::*;

            #[test]
            fn accepts() {
                let text = "é".repeat(2048);
                assert_eq!(text.len(), 4096);
                assert!(UsageTemplate::parse(&text, ResourceType::Generic).is_ok());
            }
        }

        mod on_4097_bytes {
            use super::*;

            #[test]
            fn returns_usage_too_long() {
                let text = "a".repeat(4097);
                assert_eq!(
                    UsageTemplate::parse(&text, ResourceType::Generic),
                    Err(TextError::UsageTooLong { max: 4096 })
                );
            }
        }

        mod on_a_key_not_valid_for_the_type {
            use super::*;

            #[test]
            fn returns_unknown_placeholder_with_the_placeholder_and_sorted_valid_keys() {
                let err =
                    UsageTemplate::parse("port {unit.port}", ResourceType::Device).unwrap_err();
                let TextError::UnknownPlaceholder { placeholder, valid } = err else {
                    panic!("wrong variant: {err:?}");
                };
                assert_eq!(placeholder, "unit.port");
                assert_eq!(
                    valid,
                    [
                        "resource.description",
                        "resource.id",
                        "unit.description",
                        "unit.id",
                        "unit.path"
                    ]
                );
            }
        }

        mod on_non_placeholder_braces {
            use super::*;

            #[test]
            fn accepts_without_error() {
                for text in [
                    "${PORT}",
                    "{}",
                    "{unit.}",
                    "{Unit.path}",
                    "{unit.Path}",
                    "{unit.path",
                    "{{unit.bogus-key}}",
                    "{\"a\": {\"b\": 1}}",
                ] {
                    assert!(
                        UsageTemplate::parse(text, ResourceType::Device).is_ok(),
                        "{text}"
                    );
                }
            }
        }
    }

    mod placeholder_keys {
        use super::*;

        mod for_each_type {
            use super::*;

            #[test]
            fn returns_exactly_the_documented_keys() {
                assert_eq!(
                    placeholder_keys(ResourceType::Device),
                    [
                        "resource.description",
                        "resource.id",
                        "unit.description",
                        "unit.id",
                        "unit.path"
                    ]
                );
                assert_eq!(
                    placeholder_keys(ResourceType::Service),
                    [
                        "resource.description",
                        "resource.id",
                        "unit.address",
                        "unit.description",
                        "unit.host",
                        "unit.id",
                        "unit.mode",
                        "unit.port",
                        "unit.scheme",
                        "unit.service_id"
                    ]
                );
                assert_eq!(
                    placeholder_keys(ResourceType::Generic),
                    [
                        "resource.description",
                        "resource.id",
                        "unit.description",
                        "unit.id",
                        "unit.identity_key"
                    ]
                );
            }

            #[test]
            fn keys_are_sorted() {
                for ty in [
                    ResourceType::Device,
                    ResourceType::Service,
                    ResourceType::Generic,
                ] {
                    let keys = placeholder_keys(ty);
                    assert!(keys.windows(2).all(|w| w[0] < w[1]), "{ty:?}");
                }
            }
        }
    }

    mod usage_template_render {
        use super::*;

        mod on_device_unit {
            use super::*;

            #[test]
            fn replaces_unit_path() {
                let out = render_with("open {unit.path}", ResourceType::Device, &device());
                assert_eq!(out, "open /dev/ttyUSB0");
            }

            #[test]
            fn replaces_resource_and_unit_id_and_description() {
                let out = render_with(
                    "{resource.id}|{resource.description}|{unit.id}|{unit.description}",
                    ResourceType::Device,
                    &device(),
                );
                assert_eq!(out, "rig|the rig|u1|unit one");
            }
        }

        mod on_service_unit {
            use super::*;

            #[test]
            fn renders_address_as_host_colon_port() {
                let out = render_with(
                    "{unit.address}",
                    ResourceType::Service,
                    &service("example.org", 8080),
                );
                assert_eq!(out, "example.org:8080");
            }

            #[test]
            fn renders_address_as_bracketed_host_for_ipv6() {
                let out = render_with("{unit.address}", ResourceType::Service, &service("::1", 80));
                assert_eq!(out, "[::1]:80");
            }

            #[test]
            fn renders_mode_as_connect_or_listen() {
                let mut s = service("h", 1);
                assert_eq!(
                    render_with("{unit.mode}", ResourceType::Service, &s),
                    "connect"
                );
                if let UnitSettings::Service { mode, .. } = &mut s {
                    *mode = ServiceMode::Listen;
                }
                assert_eq!(
                    render_with("{unit.mode}", ResourceType::Service, &s),
                    "listen"
                );
            }

            #[test]
            fn renders_unset_scheme_and_service_id_as_empty_text() {
                let out = render_with(
                    "[{unit.scheme}][{unit.service_id}]",
                    ResourceType::Service,
                    &service("h", 1),
                );
                assert_eq!(out, "[][]");
            }

            #[test]
            fn renders_set_scheme_service_id_host_and_port() {
                let s = UnitSettings::Service {
                    mode: ServiceMode::Connect,
                    scheme: Some("https".to_owned()),
                    host: "h".to_owned(),
                    port: 443,
                    service_id: Some("sid".to_owned()),
                };
                let out = render_with(
                    "{unit.scheme} {unit.host} {unit.port} {unit.service_id}",
                    ResourceType::Service,
                    &s,
                );
                assert_eq!(out, "https h 443 sid");
            }
        }

        mod on_generic_unit {
            use super::*;

            #[test]
            fn replaces_identity_key() {
                let s = UnitSettings::Generic {
                    identity_key: "key-1".to_owned(),
                };
                let out = render_with("id={unit.identity_key}", ResourceType::Generic, &s);
                assert_eq!(out, "id=key-1");
            }
        }

        mod on_non_placeholder_braces {
            use super::*;

            #[test]
            fn leaves_them_byte_for_byte_unchanged() {
                let text = "port=${PORT} $(echo {}) {unit.} {Unit.path} {unit.Path} {\"a\": {\"b\": [1]}} {{ é }} {unit.path";
                assert_eq!(render_with(text, ResourceType::Device, &device()), text);
            }
        }

        mod on_a_placeholder_repeated_several_times {
            use super::*;

            #[test]
            fn replaces_every_occurrence() {
                let out = render_with(
                    "{unit.path} {unit.path}{unit.path}",
                    ResourceType::Device,
                    &device(),
                );
                assert_eq!(out, "/dev/ttyUSB0 /dev/ttyUSB0/dev/ttyUSB0");
            }
        }

        mod on_a_substituted_value_that_looks_like_a_placeholder {
            use super::*;

            #[test]
            fn does_not_substitute_again() {
                let t = UsageTemplate::parse("{unit.description}", ResourceType::Device).unwrap();
                let (r, u, s) = (rid(), uid(), device());
                let out = t.render(
                    &RenderResource {
                        id: &r,
                        description: "",
                    },
                    &RenderUnit {
                        id: &u,
                        description: "{unit.path} {resource.id}",
                        settings: &s,
                    },
                );
                assert_eq!(out, "{unit.path} {resource.id}");
            }
        }
    }

    mod text_error {
        use super::*;

        mod code {
            use super::*;

            #[test]
            fn maps_each_variant_to_its_snake_case_code() {
                let cases = [
                    (
                        TextError::DescriptionTooLong { max: 200 },
                        "description_too_long",
                    ),
                    (TextError::DescriptionMultiline, "description_multiline"),
                    (TextError::UsageTooLong { max: 4096 }, "usage_too_long"),
                    (
                        TextError::UnknownPlaceholder {
                            placeholder: "unit.x".to_owned(),
                            valid: vec![],
                        },
                        "unknown_placeholder",
                    ),
                ];
                for (err, code) in cases {
                    assert_eq!(err.code(), code);
                }
            }
        }

        mod details {
            use super::*;

            #[test]
            fn unknown_placeholder_carries_placeholder_and_valid_keys() {
                let err = TextError::UnknownPlaceholder {
                    placeholder: "unit.pth".to_owned(),
                    valid: vec!["unit.path".to_owned(), "unit.id".to_owned()],
                };
                assert_eq!(
                    err.details(),
                    Some(json!({"placeholder": "unit.pth", "valid": ["unit.path", "unit.id"]}))
                );
            }

            #[test]
            fn other_variants_carry_no_details() {
                assert_eq!(TextError::DescriptionMultiline.details(), None);
            }
        }
    }
}
