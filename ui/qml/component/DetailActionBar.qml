pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import Qcm.Material as MD

MD.ActionToolBar {
    id: root

    Layout.fillWidth: true
    Layout.preferredWidth: Math.ceil(maximumContentWidth) + 2
    Layout.maximumWidth: Layout.preferredWidth
    Layout.alignment: Qt.AlignVCenter

    component DetailActionButton: MD.ActionIconButton {
        id: button
        mdState.size: MD.Enum.XS

        readonly property string toolTipText: button.action?.tooltip || button.action?.text || ""

        hoverEnabled: true
        MD.ToolTip.text: button.toolTipText
        MD.ToolTip.visible: button.mdState.interactions.hovered && button.toolTipText.length > 0 && !button.mdState.interactions.pressed
    }

    iconDelegate: DetailActionButton {
        action: MD.ToolBarLayout.action
    }
    moreDelegate: DetailActionButton {
        action: root.moreAction
    }
}
