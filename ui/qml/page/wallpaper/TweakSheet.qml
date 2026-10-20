pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import Qcm.Material as MD
import waywallen.ui as W

MD.BottomSheet {
    id: control

    required property Item popupParent
    required property var tweak

    parent: popupParent
    z: 25
    sheetType: MD.Enum.BottomSheetModal
    dim: false
    dismissOnDragDown: true
    maxSheetWidth: 560

    ColumnLayout {
        width: control.sheetWidth
        spacing: 0

        ColumnLayout {
            Layout.fillWidth: true
            Layout.leftMargin: 16
            Layout.rightMargin: 16
            Layout.bottomMargin: 16
            spacing: 16

            MD.Text {
                Layout.fillWidth: true
                text: qsTr("Tweak")
                typescale: MD.Token.typescale.title_medium
                color: MD.Token.color.on_surface
                maximumLineCount: 1
                elide: Text.ElideRight
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 8

                MD.Text {
                    Layout.fillWidth: true
                    text: qsTr("Aspect ratio")
                    typescale: MD.Token.typescale.label_medium
                    color: MD.Token.color.on_surface_variant
                }

                MD.SegmentedButtonGroup {
                    size: MD.Enum.XS

                    MD.SegmentedButton {
                        text: "1:1"
                        checked: Math.abs(control.tweak.itemAspectRatio - 1) < 0.001
                        onCheckedChangeRequested: control.tweak.setItemAspectRatio(1)
                    }

                    MD.SegmentedButton {
                        text: "4:3"
                        checked: Math.abs(control.tweak.itemAspectRatio - 4 / 3) < 0.001
                        onCheckedChangeRequested: control.tweak.setItemAspectRatio(4 / 3)
                    }

                    MD.SegmentedButton {
                        text: "16:9"
                        checked: Math.abs(control.tweak.itemAspectRatio - 16 / 9) < 0.001
                        onCheckedChangeRequested: control.tweak.setItemAspectRatio(16 / 9)
                    }

                    MD.SegmentedButton {
                        text: "9:16"
                        checked: Math.abs(control.tweak.itemAspectRatio - 9 / 16) < 0.001
                        onCheckedChangeRequested: control.tweak.setItemAspectRatio(9 / 16)
                    }
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 4

                RowLayout {
                    Layout.fillWidth: true

                    MD.Text {
                        Layout.fillWidth: true
                        text: qsTr("Size")
                        typescale: MD.Token.typescale.label_medium
                        color: MD.Token.color.on_surface_variant
                    }
                }

                W.ValueSlider {
                    Layout.fillWidth: true
                    from: control.tweak.minimumItemSize
                    to: control.tweak.maximumItemSize
                    stepSize: control.tweak.itemSizeStep
                    snapMode: MD.Slider.SnapAlways
                    value: control.tweak.itemSize
                    valueText: qsTr("%1 px").arg(Math.round(value))
                    valueMaxText: qsTr("%1 px").arg(Math.round(to))
                    onValueChangeRequested: value => control.tweak.setItemSize(value)
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 8

                MD.Text {
                    Layout.fillWidth: true
                    text: qsTr("Fill mode")
                    typescale: MD.Token.typescale.label_medium
                    color: MD.Token.color.on_surface_variant
                }

                MD.SegmentedButtonGroup {
                    size: MD.Enum.XS

                    MD.SegmentedButton {
                        text: qsTr("Fill cell")
                        checked: control.tweak.layoutMode === control.tweak.layoutFillCell
                        onCheckedChangeRequested: control.tweak.setLayoutMode(control.tweak.layoutFillCell)
                    }

                    MD.SegmentedButton {
                        text: qsTr("Fixed")
                        checked: control.tweak.layoutMode === control.tweak.layoutFixed
                        onCheckedChangeRequested: control.tweak.setLayoutMode(control.tweak.layoutFixed)
                    }
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 4

                MD.Text {
                    text: qsTr("Image decode cache")
                    typescale: MD.Token.typescale.label_medium
                    color: MD.Token.color.on_surface_variant
                }

                W.ValueSlider {
                    Layout.fillWidth: true
                    from: 0
                    to: control.tweak.imageCacheOptions.length - 1
                    stepSize: 1
                    snapMode: MD.Slider.SnapAlways
                    value: Math.max(0, control.tweak.imageCacheOptions.indexOf(control.tweak.imageCacheMiB))
                    valueText: qsTr("%1 MiB").arg(control.tweak.imageCacheOptions[Math.round(value)])
                    valueMaxText: qsTr("512 MiB")
                    onValueChangeRequested: value => control.tweak.setImageCacheMiB(control.tweak.imageCacheOptions[Math.round(value)])
                }

                MD.Text {
                    Layout.fillWidth: true
                    text: qsTr("Lower values use less memory but may increase CPU usage when playing animated thumbnails. Set to 0 to disable caching.")
                    wrapMode: Text.WordWrap
                    typescale: MD.Token.typescale.body_small
                    color: MD.Token.color.on_surface_variant
                }
            }
        }
    }
}
