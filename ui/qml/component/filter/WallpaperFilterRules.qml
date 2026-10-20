pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import waywallen.control as WC
import waywallen.ui as W
import Qcm.Material as MD

MD.VerticalListView {
    id: root

    required property var popupWindow
    property var supportedTypes: []
    property var typeLabels: ({})
    property var allTags: []
    property var valueLabels: ({})
    property var allContentRatings: []
    property color ruleBackgroundColor: root.MD.MProp.color.surface

    spacing: 2

    delegate: W.WallpaperFilter {
        width: ListView.view.width
        popupWindow: root.popupWindow
        supportedTypes: root.supportedTypes
        typeLabels: root.typeLabels
        allTags: root.allTags
        valueLabels: root.valueLabels
        allContentRatings: root.allContentRatings
        backgroundColor: root.ruleBackgroundColor
    }

    section.property: "group"
    section.criteria: ViewSection.FullString
    section.delegate: RowLayout {
        id: sectionRow
        width: ListView.view.contentWidth
        spacing: 8
        required property string section
        readonly property int groupId: parseInt(section, 10)
        readonly property int sectionIndex: {
            void (root.count);
            return root.model ? root.model.sectionIndexForGroup(groupId) : -1;
        }
        readonly property int currentOp: {
            if (!root.model || sectionIndex <= 0)
                return -1;
            void (root.model.filterLogics);
            return root.model.logicOpAt(sectionIndex);
        }

        MD.Label {
            Layout.fillWidth: true
            text: qsTranslate("WallpaperFilterDialog", "Group %1").arg(sectionRow.sectionIndex + 1)
            typescale: MD.Token.typescale.label_medium
        }

        MD.SegmentedButtonGroup {
            visible: sectionRow.sectionIndex > 0

            MD.SegmentedButton {
                mdState.size: MD.Enum.XS
                text: qsTranslate("WallpaperFilterDialog", "AND")
                checked: sectionRow.currentOp !== WC.LogicOp.LOGIC_OP_OR
                onCheckedChangeRequested: root.model.setLogicOpAt(sectionRow.sectionIndex, WC.LogicOp.LOGIC_OP_AND)
            }

            MD.SegmentedButton {
                mdState.size: MD.Enum.XS
                text: qsTranslate("WallpaperFilterDialog", "OR")
                checked: sectionRow.currentOp === WC.LogicOp.LOGIC_OP_OR
                onCheckedChangeRequested: root.model.setLogicOpAt(sectionRow.sectionIndex, WC.LogicOp.LOGIC_OP_OR)
            }
        }

        MD.IconButton {
            mdState.size: MD.Enum.XS
            icon.name: MD.Token.icon.add
            Accessible.name: qsTr("Add rule")
            MD.ToolTip.visible: mdState.interactions.hovered
            MD.ToolTip.text: qsTr("Add rule")
            onClicked: root.model.appendRuleInGroup(sectionRow.groupId)
        }
        MD.IconButton {
            mdState.size: MD.Enum.XS
            icon.name: MD.Token.icon.delete
            Accessible.name: qsTr("Delete group")
            MD.ToolTip.visible: mdState.interactions.hovered
            MD.ToolTip.text: qsTr("Delete group")
            onClicked: root.model.deleteGroup(sectionRow.groupId)
        }
    }
}
