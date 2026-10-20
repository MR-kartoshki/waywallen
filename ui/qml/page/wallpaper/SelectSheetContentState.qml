pragma ComponentBehavior: Bound
import QtQml

QtObject {
    id: root

    required property var page
    required property var playlistListQuery
    required property var playlistMutation

    readonly property int selectedWallpaperCount: page.selectedWallpaperCount
    readonly property bool playlistListLoading: page.playlistListLoading
    readonly property var playlists: (playlistListQuery.playlists || []).filter(playlist => playlist.sourceKind !== "smart")
    readonly property bool mutationQuerying: playlistMutation.querying

    function createPlaylist(name) {
        page.createPlaylistFromSelection(name);
    }

    function addToPlaylist(playlist) {
        if (playlist.sourceKind === "smart")
            return;
        page.addSelectionToPlaylist(playlist);
    }
}
