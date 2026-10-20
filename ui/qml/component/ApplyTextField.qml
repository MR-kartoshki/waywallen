pragma ComponentBehavior: Bound
import QtQuick
import Qcm.Material as MD

MD.TextField {
    id: root

    property bool canApply: true
    property string applyText: qsTr("Apply")

    signal applied(string value)

    mdState.size: MD.Enum.S
    onAccepted: submit()

    function submit() {
        if (enabled && canApply && acceptableInput)
            root.applied(text);
    }

    trailing: MD.IconButton {
        mdState.size: MD.Enum.XS
        anchors.right: parent?.right
        anchors.verticalCenter: parent?.verticalCenter
        anchors.rightMargin: 8
        icon.name: MD.Token.icon.check
        enabled: root.canApply && root.acceptableInput
        Accessible.name: root.applyText
        onClicked: root.submit()
        MD.ToolTip.visible: mdState.interactions.hovered
        MD.ToolTip.text: root.applyText
    }
}
