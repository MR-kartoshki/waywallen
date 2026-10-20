use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum PlaylistMode {
    #[sea_orm(string_value = "sequential")]
    Sequential,
    #[sea_orm(string_value = "shuffle")]
    Shuffle,
    #[sea_orm(string_value = "random")]
    Random,
}

impl From<crate::playback::Mode> for PlaylistMode {
    fn from(m: crate::playback::Mode) -> Self {
        match m {
            crate::playback::Mode::Sequential => PlaylistMode::Sequential,
            crate::playback::Mode::Shuffle => PlaylistMode::Shuffle,
            crate::playback::Mode::Random => PlaylistMode::Random,
        }
    }
}

impl From<PlaylistMode> for crate::playback::Mode {
    fn from(m: PlaylistMode) -> Self {
        match m {
            PlaylistMode::Sequential => crate::playback::Mode::Sequential,
            PlaylistMode::Shuffle => crate::playback::Mode::Shuffle,
            PlaylistMode::Random => crate::playback::Mode::Random,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "playlist")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub smart_selection: Option<String>,
    pub mode: PlaylistMode,
    pub interval_secs: i64,
    pub synchronized_selection: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::playlist_item::Entity")]
    PlaylistItem,
}

impl Related<super::playlist_item::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PlaylistItem.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
