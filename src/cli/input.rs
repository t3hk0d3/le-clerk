//! YAML input for the inventory commands (`-f PATH` or `-f -`).
//!
//! Stdin is read only for `-f -`, to EOF. Input is capped at
//! [`MAX_INPUT`]. Plain scalars that YAML reads as numbers or booleans are
//! refused for string fields (quote them), never coerced.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use thiserror::Error;

/// Largest document accepted, in bytes.
pub const MAX_INPUT: u64 = 64 * 1024;

/// Where a document comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSpec {
    /// No `-f`.
    None,
    /// `-f -`.
    Stdin,
    /// `-f PATH`.
    Path(PathBuf),
}

impl InputSpec {
    /// The spec for an optional `-f` value.
    #[must_use]
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            None => Self::None,
            Some("-") => Self::Stdin,
            Some(path) => Self::Path(PathBuf::from(path)),
        }
    }

    fn name(&self) -> String {
        match self {
            Self::None => "nothing".to_owned(),
            Self::Stdin => "stdin".to_owned(),
            Self::Path(p) => p.display().to_string(),
        }
    }
}

/// Why a document could not be used.
#[derive(Debug, Error)]
pub enum InputError {
    /// `-f PATH` names no file.
    #[error("Cannot read the YAML file {0}: it does not exist. Check the path given to -f.")]
    Missing(PathBuf),
    /// Reading failed.
    #[error("Cannot read YAML from {source_name}: {error}.")]
    Read {
        source_name: String,
        error: io::Error,
    },
    /// More than [`MAX_INPUT`] bytes.
    #[error("The YAML from {0} is larger than 64 KiB; shorten it.")]
    TooLarge(String),
    /// Not UTF-8 text.
    #[error("The YAML from {0} is not UTF-8 text.")]
    NotUtf8(String),
    /// Malformed, not one mapping, an unknown key or a bad value.
    #[error("Invalid YAML from {source_name}: {message}. Fix the document and retry.")]
    Parse {
        source_name: String,
        message: String,
    },
}

/// Read and parse the document `spec` names; `None` without `-f`.
///
/// # Errors
/// [`InputError`] when the document cannot be read or does not fit `T`.
pub fn load<T: DeserializeOwned>(spec: &InputSpec) -> Result<Option<T>, InputError> {
    let source_name = spec.name();
    let bytes = match spec {
        InputSpec::None => return Ok(None),
        InputSpec::Stdin => read_capped(io::stdin().lock(), &source_name)?,
        InputSpec::Path(path) => {
            let file = File::open(path).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    InputError::Missing(path.clone())
                } else {
                    InputError::Read {
                        source_name: source_name.clone(),
                        error,
                    }
                }
            })?;
            read_capped(file, &source_name)?
        }
    };
    let text = String::from_utf8(bytes).map_err(|_| InputError::NotUtf8(source_name.clone()))?;
    parse(&text, &source_name).map(Some)
}

fn read_capped(reader: impl Read, source_name: &str) -> Result<Vec<u8>, InputError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| InputError::Read {
            source_name: source_name.to_owned(),
            error,
        })?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(InputError::TooLarge(source_name.to_owned()));
    }
    Ok(bytes)
}

/// Parse one YAML document into `T`.
fn parse<T: DeserializeOwned>(text: &str, source_name: &str) -> Result<T, InputError> {
    // `no_schema` refuses plain scalars such as `2024` or `no` for string
    // fields ("must be quoted") instead of turning them into text.
    let options = serde_saphyr::options! {
        no_schema: true,
        with_snippet: false,
    };
    serde_saphyr::from_str_with_options(text, options).map_err(|e| InputError::Parse {
        source_name: source_name.to_owned(),
        message: e
            .to_string()
            .replace(
                "multiple YAML documents detected; use from_str_multiple or \
                 from_str_multiple_with_options",
                "more than one YAML document; give exactly one",
            )
            .trim_end_matches('.')
            .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        usage: Option<String>,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        listen: Option<bool>,
    }

    fn message(text: &str) -> String {
        parse::<Doc>(text, "stdin").unwrap_err().to_string()
    }

    #[test]
    fn reads_a_block_scalar_with_its_trailing_newline() {
        let doc: Doc = parse("usage: |\n  a\n   b\n", "stdin").unwrap();
        assert_eq!(doc.usage.as_deref(), Some("a\n b\n"));
    }

    #[test]
    fn keeps_typed_numbers_and_booleans() {
        let doc: Doc = parse("port: 80\nlisten: true\n", "stdin").unwrap();
        assert_eq!((doc.port, doc.listen), (Some(80), Some(true)));
    }

    #[test]
    fn refuses_a_plain_number_tag_naming_it() {
        let m = message("tags: [esp32, 2024]\n");
        assert!(m.contains("[2024] must be quoted"), "{m}");
    }

    #[test]
    fn refuses_a_yaml_1_1_boolean_tag() {
        assert!(message("tags: [no]\n").contains("[no] must be quoted"));
    }

    #[test]
    fn accepts_quoted_scalars() {
        let doc: Doc = parse("tags: ['2024', \"no\"]\n", "stdin").unwrap();
        assert_eq!(doc.tags, Some(vec!["2024".to_owned(), "no".to_owned()]));
    }

    #[test]
    fn names_an_unknown_key() {
        assert!(message("colour: red\n").contains("unknown field `colour`"));
    }

    #[test]
    fn refuses_two_documents() {
        let m = message("usage: a\n---\nusage: b\n");
        assert!(m.contains("more than one YAML document"), "{m}");
    }

    #[test]
    fn caps_the_input() {
        let big = vec![b'x'; 65 * 1024];
        assert!(matches!(
            read_capped(big.as_slice(), "stdin"),
            Err(InputError::TooLarge(_))
        ));
    }

    #[test]
    fn missing_file_names_the_path() {
        let spec = InputSpec::Path(PathBuf::from("/no/such/clerk.yaml"));
        let err = load::<Doc>(&spec).unwrap_err();
        assert!(err.to_string().contains("/no/such/clerk.yaml"));
    }

    #[test]
    fn no_file_flag_reads_nothing() {
        assert_eq!(load::<Doc>(&InputSpec::None).unwrap(), None);
    }
}
