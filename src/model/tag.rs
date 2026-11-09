//! Resource search tags: a strict format and a deduplicated, sorted set.
//!
//! Pure rules: nothing here touches the store or the clock. Tags and tag sets
//! only serialise; wire arguments carry plain strings so callers can report
//! `invalid_tag` for the offending input.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::error::ErrorCode;

/// Longest tag, in characters.
pub const TAG_MAX_CHARS: usize = 32;
/// Most distinct tags on one resource.
pub const TAGS_MAX: usize = 16;

/// Why a tag or tag set was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TagError {
    /// The text is not a valid tag.
    #[error(
        "invalid tag {tag:?}: use 1-{TAG_MAX_CHARS} lowercase letters, digits or '-', starting with a letter or digit"
    )]
    Invalid {
        /// The input exactly as given.
        tag: String,
    },
    /// The set has more than [`TAGS_MAX`] distinct tags.
    #[error("too many tags: {count} (at most {max})")]
    TooMany {
        /// The limit.
        max: usize,
        /// The number of distinct valid tags.
        count: usize,
    },
}

impl ErrorCode for TagError {
    fn code(&self) -> &'static str {
        match self {
            TagError::Invalid { .. } => "invalid_tag",
            TagError::TooMany { .. } => "too_many_tags",
        }
    }

    fn details(&self) -> Option<Value> {
        match self {
            TagError::Invalid { tag } => Some(json!({ "tag": tag })),
            TagError::TooMany { max, count } => Some(json!({ "max": max, "count": count })),
        }
    }
}

/// One search tag: lowercase ASCII letters, digits and `-`, 1 to 32
/// characters, starting with a letter or digit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Tag(String);

impl Tag {
    /// Validate `text` as a tag. Never lowercases or trims.
    ///
    /// # Errors
    /// [`TagError::Invalid`] carrying `text` as given.
    pub fn parse(text: &str) -> Result<Self, TagError> {
        let mut chars = text.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        // ASCII-only after the checks above, so byte length is character length.
        if first_ok && rest_ok && text.len() <= TAG_MAX_CHARS {
            Ok(Self(text.to_owned()))
        } else {
            Err(TagError::Invalid {
                tag: text.to_owned(),
            })
        }
    }

    /// The tag as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A sorted set of at most [`TAGS_MAX`] distinct tags; empty by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TagSet(BTreeSet<Tag>);

impl TagSet {
    /// Validate `texts` as a tag set; duplicates collapse to one.
    ///
    /// # Errors
    /// [`TagError::Invalid`] for the first malformed entry in input order;
    /// otherwise [`TagError::TooMany`] when more than [`TAGS_MAX`] distinct
    /// tags remain.
    pub fn parse<I, S>(texts: I) -> Result<Self, TagError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = BTreeSet::new();
        for text in texts {
            set.insert(Tag::parse(text.as_ref())?);
        }
        if set.len() > TAGS_MAX {
            return Err(TagError::TooMany {
                max: TAGS_MAX,
                count: set.len(),
            });
        }
        Ok(Self(set))
    }

    /// Wrap tags read back from storage.
    ///
    /// Only for text that passed [`Tag::parse`] when stored; no check is
    /// repeated here.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the store reads tags through it from the next task"
        )
    )]
    pub(crate) fn from_stored(tags: Vec<String>) -> Self {
        Self(tags.into_iter().map(Tag).collect())
    }

    /// The tags in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &Tag> {
        self.0.iter()
    }

    /// The number of tags.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether every tag of `filter` is in this set; true for an empty filter.
    #[must_use]
    pub fn contains_all(&self, filter: &TagSet) -> bool {
        filter.0.is_subset(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(set: &TagSet) -> Vec<&str> {
        set.iter().map(Tag::as_str).collect()
    }

    fn many(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i:02}")).collect()
    }

    mod tag_parse {
        use super::*;

        mod on_a_valid_tag {
            use super::*;

            #[test]
            fn returns_the_tag_with_as_str_equal_to_the_input() {
                let max = "a".repeat(32);
                for text in ["esp32c6", "thread", "2-4ghz", "0", "a-", max.as_str()] {
                    assert_eq!(Tag::parse(text).unwrap().as_str(), text);
                }
            }
        }

        mod on_an_invalid_tag {
            use super::*;

            #[test]
            fn returns_invalid_carrying_the_input_as_given() {
                let long = "a".repeat(33);
                for text in [
                    "",
                    long.as_str(),
                    "Thread",
                    "esp32 c6",
                    "zigbee,thread",
                    "a_b",
                    "-x",
                    "café",
                    " thread",
                    "thread\n",
                ] {
                    assert_eq!(
                        Tag::parse(text),
                        Err(TagError::Invalid {
                            tag: text.to_owned()
                        }),
                        "{text:?}"
                    );
                }
            }
        }
    }

    mod tag_set_parse {
        use super::*;

        mod on_an_empty_list {
            use super::*;

            #[test]
            fn returns_an_empty_set() {
                let set = TagSet::parse(Vec::<&str>::new()).unwrap();
                assert!(set.is_empty());
                assert_eq!(set.len(), 0);
                assert_eq!(set, TagSet::default());
            }
        }

        mod on_duplicates_and_unsorted_input {
            use super::*;

            #[test]
            fn returns_each_tag_once_sorted() {
                let set = TagSet::parse(["thread", "esp32c6", "thread", "2-4ghz"]).unwrap();
                assert_eq!(strs(&set), ["2-4ghz", "esp32c6", "thread"]);
                assert_eq!(set.len(), 3);
            }
        }

        mod on_16_distinct_tags_with_duplicates_in_the_input {
            use super::*;

            #[test]
            fn accepts() {
                let mut texts = many(16);
                texts.extend(many(5));
                assert_eq!(texts.len(), 21);
                assert_eq!(TagSet::parse(texts).unwrap().len(), 16);
            }
        }

        mod on_17_distinct_tags {
            use super::*;

            #[test]
            fn returns_too_many_with_max_16_and_count_17() {
                assert_eq!(
                    TagSet::parse(many(17)),
                    Err(TagError::TooMany { max: 16, count: 17 })
                );
            }
        }

        mod on_one_malformed_entry {
            use super::*;

            #[test]
            fn returns_invalid_for_that_entry() {
                assert_eq!(
                    TagSet::parse(["thread", "Zigbee", "esp32c6"]),
                    Err(TagError::Invalid {
                        tag: "Zigbee".to_owned()
                    })
                );
            }

            #[test]
            fn reports_the_first_malformed_entry_in_input_order() {
                assert_eq!(
                    TagSet::parse(["ok", "B", "A"]),
                    Err(TagError::Invalid {
                        tag: "B".to_owned()
                    })
                );
            }

            #[test]
            fn reports_it_before_counting_too_many() {
                let mut texts = many(17);
                texts.push("Bad".to_owned());
                assert_eq!(
                    TagSet::parse(texts),
                    Err(TagError::Invalid {
                        tag: "Bad".to_owned()
                    })
                );
            }
        }
    }

    mod tag_set_contains_all {
        use super::*;

        fn set(texts: &[&str]) -> TagSet {
            TagSet::parse(texts).unwrap()
        }

        mod on_an_empty_filter {
            use super::*;

            #[test]
            fn returns_true() {
                assert!(set(&["a"]).contains_all(&TagSet::default()));
                assert!(TagSet::default().contains_all(&TagSet::default()));
            }
        }

        mod on_a_filter_that_is_a_subset {
            use super::*;

            #[test]
            fn returns_true() {
                assert!(set(&["a", "b", "c"]).contains_all(&set(&["a", "c"])));
                assert!(set(&["a", "b"]).contains_all(&set(&["a", "b"])));
            }
        }

        mod on_a_filter_with_one_missing_tag {
            use super::*;

            #[test]
            fn returns_false() {
                assert!(!set(&["a", "b"]).contains_all(&set(&["a", "z"])));
                assert!(!TagSet::default().contains_all(&set(&["a"])));
            }
        }
    }

    mod tag_set_serialisation {
        use super::*;

        #[test]
        fn serialises_as_a_sorted_json_array_of_strings() {
            let set = TagSet::parse(["thread", "esp32c6", "thread"]).unwrap();
            assert_eq!(
                serde_json::to_value(&set).unwrap(),
                json!(["esp32c6", "thread"])
            );
            assert_eq!(serde_json::to_value(TagSet::default()).unwrap(), json!([]));
        }

        #[test]
        fn serialises_a_tag_as_a_string() {
            assert_eq!(
                serde_json::to_value(Tag::parse("thread").unwrap()).unwrap(),
                json!("thread")
            );
        }
    }

    mod tag_set_from_stored {
        use super::*;

        #[test]
        fn wraps_stored_tags_without_changing_them() {
            let set = TagSet::from_stored(vec!["a".to_owned(), "b".to_owned()]);
            assert_eq!(strs(&set), ["a", "b"]);
        }
    }

    mod tag_error {
        use super::*;

        mod code {
            use super::*;

            #[test]
            fn maps_invalid_to_invalid_tag_with_the_tag() {
                let err = TagError::Invalid {
                    tag: "Bad Tag".to_owned(),
                };
                assert_eq!(err.code(), "invalid_tag");
                assert_eq!(err.details(), Some(json!({"tag": "Bad Tag"})));
            }

            #[test]
            fn maps_too_many_to_too_many_tags_with_max_and_count() {
                let err = TagError::TooMany { max: 16, count: 17 };
                assert_eq!(err.code(), "too_many_tags");
                assert_eq!(err.details(), Some(json!({"max": 16, "count": 17})));
            }
        }
    }
}
