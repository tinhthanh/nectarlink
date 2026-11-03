// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import app.nectarlink

// What NectarLink may use on a phone on the USB cable, chosen here and granted
// from this PC over adb (usb_setup.rs), so the phone shows no permission prompts.
Card {
    id: accessCard
    /** Show it even when everything is granted (the setup walkthrough). */
    property bool always: false
    padding: 16
    readonly property var usb: {
        try { return JSON.parse(AppController.usbSetup) } catch (e) { return ({}) }
    }
    readonly property var groups: [
        { id: "notifications", title: qsTr("Notifications"), detail: qsTr("See the phone's notifications here and reply to them.") },
        { id: "calls", title: qsTr("Calls"), detail: qsTr("Call alerts, answering, and calling from this PC. Uses your contacts for names.") },
        { id: "messages", title: qsTr("Messages"), detail: qsTr("Read and send text messages from this PC.") },
        { id: "photos", title: qsTr("Photos and videos"), detail: qsTr("Browse and copy the phone's photos.") },
        { id: "cameraMic", title: qsTr("Camera and microphone"), detail: qsTr("Use the phone as this PC's webcam and microphone.") },
        { id: "background", title: qsTr("Run in the background"), detail: qsTr("Stay connected when the app isn't open (no battery optimization).") }
    ]
    readonly property var phones: (usb.phones || []).filter(p =>
        p.state === "device" && p.app === "installed" && p.access && Object.keys(p.access).length > 0
        && (accessCard.always || Object.keys(p.access).some(k => !p.access[k])))
    visible: phones.length > 0

    Column {
        width: parent.width
        spacing: 12

        Repeater {
            model: accessCard.phones
            delegate: Column {
                required property var modelData
                readonly property var phone: modelData
                width: accessCard.width - 32
                spacing: 10

                Txt {
                    width: parent.width
                    role: "title"
                    wrapMode: Text.WordWrap
                    text: qsTr("What NectarLink can use on %1").arg(phone.model.length > 0 ? phone.model : qsTr("your phone"))
                }
                Txt {
                    width: parent.width
                    role: "bodySmall"
                    muted: true
                    wrapMode: Text.WordWrap
                    text: qsTr("Turn on what you want. This PC sets it on the phone over the cable, so the phone doesn't ask.")
                }
                Repeater {
                    model: accessCard.groups
                    delegate: Item {
                        required property var modelData
                        width: parent.width
                        height: Math.max(groupText.implicitHeight, groupToggle.implicitHeight)
                        Column {
                            id: groupText
                            anchors.left: parent.left
                            anchors.right: groupToggle.left
                            anchors.rightMargin: 16
                            anchors.verticalCenter: parent.verticalCenter
                            spacing: 2
                            Txt { width: parent.width; role: "label"; text: modelData.title }
                            Txt { width: parent.width; role: "bodySmall"; muted: true; wrapMode: Text.WordWrap; text: modelData.detail }
                        }
                        Toggle {
                            id: groupToggle
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            enabled: !phone.busy
                            label: modelData.title
                            checked: phone.access[modelData.id] === true
                            onToggled: (on) => AppController.usbSetupAction(phone.serial, (on ? "allow:" : "deny:") + modelData.id)
                        }
                    }
                }
                Txt {
                    width: parent.width
                    visible: phone.error.length > 0
                    role: "bodySmall"
                    color: Theme.error
                    wrapMode: Text.WordWrap
                    text: phone.error
                }
            }
        }
    }
}
