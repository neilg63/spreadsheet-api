use serde::{Deserialize, Serialize};

/// The parts of a collection the upload and configure flows need.
#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub id: String,
    pub name: String,
    pub schema_id: Option<String>,
}

/// Who may read a collection's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Anyone who has a dataset's id.
    Unlisted,
    /// The same, and free to be listed.
    Public,
    /// The owner, or an access token that grants `read`.
    Private,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unlisted => "unlisted",
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "public" => Self::Public,
            "private" => Self::Private,
            _ => Self::Unlisted,
        }
    }
}
