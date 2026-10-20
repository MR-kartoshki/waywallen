pragma ComponentBehavior: Bound
import QtQuick
import QtQml as Qml
import QtQuick.Layouts
import Qcm.Material as MD

// Search affordance shaped like an `MD.InputChip` (32px tall, 8px
// radius, 1px outline) with a `TextInput` embedded inline. Plain
// Even an XS `MD.TextField` is 40px tall — too bulky next
// to chip / button rows. This keeps the toolbar visually homogeneous.
//
// `textEdited` is emitted after a 1s idle on the raw input, so
// consumers can connect it directly without their own debounce timer.
Item {
    id: root

    property alias text: m_input.text
    property string placeholderText
    // Debounce window for the outward `textEdited` signal. Per-keystroke
    // input still updates `text` immediately for any direct binding.
    property int debounceMs: 1000
    signal textEdited()

    implicitHeight: 32
    implicitWidth: 200

    Qml.Timer {
        id: m_debounce
        interval: root.debounceMs
        repeat: false
        onTriggered: root.textEdited()
    }

    Rectangle {
        id: m_bg
        anchors.fill: parent
        radius: 8
        color: "transparent"
        border.width: 1
        border.color: MD.MProp.color.outline_variant

        // Ripple/state-layer parity with chip widgets — purely
        // hover-driven since the whole chip is a focus target via
        // the embedded TextInput.
        MD.Ripple {
            anchors.fill: parent
            radius: parent.radius
            interactionSource: m_mouse.interactionSource
            stateOpacity: mouseInteractions.hovered
                          ? MD.Token.state.hover.state_layer_opacity
                          : 0
            color: MD.MProp.color.on_surface_variant
        }

        MD.FocusIndicator {
            corners: MD.Util.corners(parent.radius)
            active: m_input.activeFocus
        }

        // Clicking anywhere on the chip focuses the input.
        MD.InteractionObserver {
            id: mouseInteractions
            source: m_mouse.interactionSource
        }

        MD.Clickable {
            id: m_mouse
            anchors.fill: parent
            onClicked: m_input.forceActiveFocus()

            HoverHandler {
                cursorShape: Qt.IBeamCursor
            }
        }

        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 8
            anchors.rightMargin: m_clear.visible ? 4 : 8
            spacing: 6

            MD.Icon {
                name: MD.Token.icon.search
                size: 16
                color: MD.MProp.color.on_surface_variant
                Layout.alignment: Qt.AlignVCenter
            }

            Item {
                Layout.fillWidth: true
                Layout.alignment: Qt.AlignVCenter
                Layout.preferredHeight: m_input.implicitHeight

                MD.Label {
                    id: m_placeholder
                    anchors.fill: parent
                    verticalAlignment: Text.AlignVCenter
                    typescale: MD.Token.typescale.label_large
                    color: MD.MProp.color.on_surface_variant
                    text: root.placeholderText
                    visible: m_input.text.length === 0 && ! m_input.activeFocus
                    elide: Text.ElideRight
                    // `MD.Page` propagates `font.capitalization: Capitalize`
                    // down the tree (see qml_material/control/Page.qml).
                    // Force MixedCase locally so the placeholder + typed
                    // input render verbatim instead of title-casing every
                    // word boundary.
                    font.capitalization: Font.MixedCase
                }

                TextInput {
                    id: m_input
                    anchors.fill: parent
                    verticalAlignment: TextInput.AlignVCenter
                    color: MD.MProp.color.on_surface
                    selectionColor: MD.MProp.color.primary
                    selectedTextColor: MD.MProp.color.on_primary
                    selectByMouse: true
                    clip: true
                    // Inherits the placeholder's font wholesale, which
                    // already carries `capitalization: MixedCase`.
                    font: m_placeholder.font
                    onTextEdited: m_debounce.restart()
                    Keys.onEscapePressed: event => {
                        focus = false;
                        event.accepted = true;
                    }
                }
            }

            MD.IconButton {
                id: m_clear
                visible: m_input.text.length > 0
                icon.name: MD.Token.icon.close
                icon.width: 14
                icon.height: 14
                padding: 2
                topInset: 0
                bottomInset: 0
                leftInset: 0
                rightInset: 0
                background.implicitWidth: 22
                background.implicitHeight: 22
                Layout.alignment: Qt.AlignVCenter
                onClicked: {
                    m_input.clear();
                    // Clear is an explicit user gesture; bypass the
                    // debounce so the result list resets immediately.
                    m_debounce.stop();
                    root.textEdited();
                }
            }
        }
    }
}
