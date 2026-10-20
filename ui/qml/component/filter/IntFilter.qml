pragma ComponentBehavior: Bound
import QtQml
import QtQuick
import waywallen.control as WC
import Qcm.Material as MD

QtObject {
    id: root
    property var filter: null
    property int value: 0
    property int condition: WC.IntCondition.INT_CONDITION_UNSPECIFIED
    property WC.wallpaperIntFilter subfilter
    property bool _syncing: false

    readonly property var conditionModel: [
        { name: qsTr("equal"),         value: WC.IntCondition.INT_CONDITION_EQUAL },
        { name: qsTr("not equal"),     value: WC.IntCondition.INT_CONDITION_EQUAL_NOT },
        { name: qsTr("less"),          value: WC.IntCondition.INT_CONDITION_LESS },
        { name: qsTr("less equal"),    value: WC.IntCondition.INT_CONDITION_LESS_EQUAL },
        { name: qsTr("greater"),       value: WC.IntCondition.INT_CONDITION_GREATER },
        { name: qsTr("greater equal"), value: WC.IntCondition.INT_CONDITION_GREATER_EQUAL },
        { name: qsTr("any"),           value: WC.IntCondition.INT_CONDITION_UNSPECIFIED }
    ]

    readonly property Component valueDelegate: Component {
        MD.InputChip {
            id: valueChip
            visible: root.condition !== WC.IntCondition.INT_CONDITION_UNSPECIFIED
            text: String(root.value)
            onSelectionRequested: edit = true
            editDelegate: MD.TextInput {
                text: String(root.value)
                validator: IntValidator {}
                onAccepted: {
                    const parsed = parseInt(text, 10);
                    root.value = isNaN(parsed) ? 0 : parsed;
                    valueChip.edit = false;
                }
            }
        }
    }

    function syncFromFilter() {
        if (!filter)
            return;
        if (!filter.hasIntFilter)
            filter.intFilter = subfilter;
        const active = filter.hasIntFilter ? filter.intFilter : subfilter;
        _syncing = true;
        condition = active.condition;
        value = active.value;
        _syncing = false;
    }

    function commitToFilter() {
        if (!filter || _syncing)
            return;
        subfilter.condition = condition;
        subfilter.value = value;
        filter.intFilter = subfilter;
    }

    onFilterChanged: syncFromFilter()
    onConditionChanged: commitToFilter()
    onValueChanged: commitToFilter()
}
