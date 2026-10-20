use serde::{Deserialize, Serialize};

use crate::catalog::query::WallpaperSelection;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistSource {
    Curated(Vec<i64>),
    Smart(WallpaperSelection),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSelection {
    version: u32,
    selection: WallpaperSelection,
}

impl PlaylistSource {
    pub fn is_smart(&self) -> bool {
        matches!(self, Self::Smart(_))
    }

    pub fn encode_selection(&self) -> Result<Option<String>> {
        match self {
            Self::Curated(ids) => {
                if ids.iter().any(|id| *id <= 0)
                    || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len()
                {
                    return Err(Error::PlaylistInvalid(
                        "invalid or duplicate member ID".into(),
                    ));
                }
                Ok(None)
            }
            Self::Smart(selection) => {
                selection.validate()?;
                serde_json::to_string(&StoredSelection {
                    version: 1,
                    selection: selection.clone(),
                })
                .map(Some)
                .map_err(|e| Error::PlaylistInvalid(e.to_string()))
            }
        }
    }

    pub fn decode_selection(value: &str) -> Result<Self> {
        let stored: StoredSelection = serde_json::from_str(value)
            .map_err(|e| Error::PlaylistInvalid(format!("invalid smart selection: {e}")))?;
        if stored.version != 1 {
            return Err(Error::PlaylistInvalid(
                "unsupported smart selection version".into(),
            ));
        }
        stored.selection.validate()?;
        Ok(Self::Smart(stored.selection))
    }
}
