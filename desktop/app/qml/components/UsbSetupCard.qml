// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import app.nectarlink

// A phone on a USB cable that still needs something before NectarLink can use it
// (usb_setup.rs): adb, "Allow USB debugging" on the phone, or the app itself.
Card {
    id: usbCard
    /** Show the how-to-enable-USB-debugging steps when no phone is on the cable. */
    property bool howToWhenEmpty: DeviceList.count === 0
    padding: 16
    readonly property var usb: {
        try { return JSON.parse(AppController.usbSetup) } catch (e) { return ({}) }
    }
    readonly property bool adbMissing: usb.adb === "missing" || usb.adb === "failed" || usb.adb === "downloading"
    // A phone drops off once its app answers over USB.
    readonly property var phones: (usb.phones || []).filter(p =>
        p.state !== "device" || p.app === "missing" || (p.app === "installed" && !usb.usbConnected))
    // Nothing on the cable yet and no phone paired: say how to get one there.
    readonly property bool showHowTo: usb.adb === "ok" && (usb.phones || []).length === 0 && howToWhenEmpty
    visible: adbMissing || phones.length > 0 || showHowTo

    Column {
        width: parent.width
        spacing: 12

        Row {
            spacing: 8
            Icon { path: Icons.usb; anchors.verticalCenter: parent.verticalCenter }
            Txt { role: "title"; text: qsTr("Phone on USB"); anchors.verticalCenter: parent.verticalCenter }
        }

        // adb: needed for everything over the cable.
        Item {
            visible: usbCard.adbMissing
            width: parent.width
            height: Math.max(adbText.implicitHeight, adbButton.implicitHeight)
            Column {
                id: adbText
                anchors.left: parent.left
                anchors.right: adbButton.left
                anchors.rightMargin: 16
                anchors.verticalCenter: parent.verticalCenter
                spacing: 4
                Txt {
                    width: parent.width
                    wrapMode: Text.WordWrap
                    text: qsTr("NectarLink needs Android's adb tool to work with a phone over a USB cable.")
                }
                Txt {
                    width: parent.width
                    visible: (usbCard.usb.adbError || "").length > 0
                    role: "bodySmall"
                    color: Theme.error
                    wrapMode: Text.WordWrap
                    text: usbCard.usb.adbError || ""
                }
            }
            Button {
                id: adbButton
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                visible: usbCard.usb.canDownloadAdb === true
                size: "sm"
                iconPath: Icons.download
                busy: usbCard.usb.adb === "downloading"
                text: usbCard.usb.adb === "downloading" ? qsTr("Getting adb…") : qsTr("Get adb from Google")
                onClicked: AppController.usbSetupAction("", "download-adb")
            }
        }

        Repeater {
            model: usbCard.phones
            delegate: Item {
                required property var modelData
                readonly property var phone: modelData
                readonly property string action: phone.state === "offline" ? "reconnect"
                    : phone.state !== "device" ? ""
                    : phone.app === "missing" ? (usbCard.usb.hasApk ? "install" : "")
                    : phone.app === "installed" ? "open" : ""
                width: usbCard.width - 32
                height: Math.max(phoneText.implicitHeight, phoneButton.visible ? phoneButton.implicitHeight : 0)
                Column {
                    id: phoneText
                    anchors.left: parent.left
                    anchors.right: phoneButton.visible ? phoneButton.left : parent.right
                    anchors.rightMargin: 16
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 4
                    Txt {
                        width: parent.width
                        role: "label"
                        text: phone.model.length > 0 ? phone.model : qsTr("Android phone")
                    }
                    Txt {
                        width: parent.width
                        role: "bodySmall"
                        muted: true
                        wrapMode: Text.WordWrap
                        text: phone.state === "unauthorized"
                              ? qsTr("Unlock the phone. On “Allow USB debugging?”, tick “Always allow from this computer”, then tap Allow.")
                            : phone.state === "offline"
                              ? qsTr("The phone isn't answering over USB. Reconnect, or unplug the cable and plug it in again.")
                            : phone.state === "no permissions"
                              ? qsTr("This computer isn't allowed to use the phone's USB port. On Linux, add an udev rule for Android phones.")
                            : phone.state !== "device"
                              ? qsTr("Waiting for the phone (%1).").arg(phone.state)
                            : phone.app === "missing"
                              ? (usbCard.usb.hasApk
                                 ? qsTr("NectarLink isn't on this phone yet. Install it from here, over the cable.")
                                 : qsTr("NectarLink isn't on this phone yet. Install the NectarLink app on it."))
                            : qsTr("NectarLink is on the phone. Open it there to connect.")
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
                Button {
                    id: phoneButton
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    visible: action.length > 0
                    size: "sm"
                    busy: phone.busy
                    iconPath: action === "install" ? Icons.download : action === "reconnect" ? Icons.refresh : Icons.phone
                    text: action === "install" ? (phone.busy ? qsTr("Installing…") : qsTr("Install NectarLink"))
                        : action === "reconnect" ? qsTr("Reconnect")
                        : qsTr("Open on phone")
                    onClicked: AppController.usbSetupAction(phone.serial, action)
                }
            }
        }

        // No phone on the cable yet.
        Txt {
            visible: usbCard.showHowTo
            width: parent.width
            role: "bodySmall"
            muted: true
            wrapMode: Text.WordWrap
            text: qsTr("To use a phone over a USB cable:\n"
                       + "1. On the phone, open Settings › About phone and tap Build number 7 times "
                       + "(Samsung: About phone › Software information; Xiaomi: MIUI version).\n"
                       + "2. Open Developer options and turn on USB debugging "
                       + "(Xiaomi: also USB debugging (Security settings)).\n"
                       + "3. Plug the phone in with a data cable. If the phone asks what the USB is for, choose File transfer.")
        }
    }
}
