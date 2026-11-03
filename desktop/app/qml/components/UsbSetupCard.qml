// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import app.nectarlink

// Setting a phone up over the USB cable (usb_setup.rs), one thing at a time:
// plug it in, turn on USB debugging, tap Allow. Getting adb, installing the app,
// opening it and pairing happen by themselves; this says what's going on.
Card {
    id: usbCard
    /** Say "plug your phone in" when nothing is on the cable (no phone paired yet). */
    property bool howToWhenEmpty: DeviceList.count === 0
    padding: 16
    readonly property var usb: {
        try { return JSON.parse(AppController.usbSetup) } catch (e) { return ({}) }
    }
    readonly property bool adbBusy: usb.adb === "downloading"
    readonly property bool adbFailed: usb.adb === "failed" || (usb.adb === "missing" && usb.canDownloadAdb !== true)
    readonly property var cabled: usb.cabled || []
    // A phone drops off once its app answers over USB.
    readonly property var phones: (usb.phones || []).filter(p =>
        p.state !== "device" || p.app === "missing" || (p.app === "installed" && !usb.usbConnected))
    readonly property bool nothingPlugged: cabled.length === 0 && (usb.phones || []).length === 0
    readonly property bool showPlugIn: nothingPlugged && howToWhenEmpty && !usb.usbConnected
    visible: adbBusy || adbFailed || cabled.length > 0 || phones.length > 0 || showPlugIn

    // Steps to USB debugging, by maker (the vendor ID on the cable).
    function debuggingSteps(maker) {
        const about = maker === "samsung" ? qsTr("Settings › About phone › Software information, tap Build number 7 times")
            : maker === "xiaomi" ? qsTr("Settings › About phone, tap OS version (MIUI / HyperOS) 7 times")
            : maker === "huawei" ? qsTr("Settings › About phone, tap Build number 7 times")
            : maker === "oppo" || maker === "oneplus" ? qsTr("Settings › About device › Version, tap Build number 7 times")
            : maker === "vivo" ? qsTr("Settings › About phone › Software version, tap it 7 times")
            : qsTr("Settings › About phone, tap Build number 7 times")
        const extra = maker === "xiaomi" ? qsTr("\nAlso turn on USB debugging (Security settings) to control the phone from here.") : ""
        return qsTr("1. %1.\n2. Settings › System › Developer options: turn on USB debugging.").arg(about) + extra
    }

    Column {
        width: parent.width
        spacing: 12

        Row {
            spacing: 8
            Icon { path: Icons.usb; anchors.verticalCenter: parent.verticalCenter }
            Txt { role: "title"; text: qsTr("Phone on USB"); anchors.verticalCenter: parent.verticalCenter }
        }

        // adb: fetched by itself once a phone is on the cable.
        Txt {
            visible: usbCard.adbBusy
            width: parent.width
            wrapMode: Text.WordWrap
            text: qsTr("Getting Android's USB tool (adb) from Google…")
        }
        Item {
            visible: usbCard.adbFailed
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
                    text: qsTr("NectarLink couldn't get Android's USB tool (adb).")
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
                iconPath: Icons.refresh
                text: qsTr("Try again")
                onClicked: AppController.usbSetupAction("", "download-adb")
            }
        }

        // Nothing on the cable.
        Txt {
            visible: usbCard.showPlugIn && !usbCard.adbBusy && !usbCard.adbFailed
            width: parent.width
            wrapMode: Text.WordWrap
            text: qsTr("Plug your Android phone into this PC with a USB cable.")
        }

        // On the cable, USB debugging still off.
        Repeater {
            model: usbCard.cabled
            delegate: Item {
                required property var modelData
                width: usbCard.width - 32
                height: Math.max(cabledText.implicitHeight, checkButton.implicitHeight)
                Column {
                    id: cabledText
                    anchors.left: parent.left
                    anchors.right: checkButton.left
                    anchors.rightMargin: 16
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: 4
                    Txt {
                        width: parent.width
                        role: "label"
                        text: qsTr("Turn on USB debugging on %1").arg(modelData.name)
                    }
                    Txt {
                        width: parent.width
                        role: "bodySmall"
                        muted: true
                        wrapMode: Text.WordWrap
                        text: usbCard.debuggingSteps(modelData.maker)
                    }
                }
                Button {
                    id: checkButton
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    size: "sm"
                    iconPath: Icons.refresh
                    text: qsTr("Check again")
                    onClicked: AppController.usbSetupAction("", "recheck")
                }
            }
        }

        // In adb: Allow, installing, opening.
        Repeater {
            model: usbCard.phones
            delegate: Item {
                required property var modelData
                readonly property var phone: modelData
                // Installing and opening happen by themselves; the button is for a retry.
                readonly property string action: phone.state === "offline" ? "reconnect"
                    : phone.state !== "device" || phone.busy ? ""
                    : phone.app === "missing" && phone.error.length > 0 && usbCard.usb.hasApk ? "install"
                    : phone.app === "installed" && phone.error.length > 0 ? "open" : ""
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
                              ? qsTr("On the phone, tick “Always allow from this computer” and tap Allow.")
                            : phone.state === "offline"
                              ? qsTr("The phone isn't answering over USB. Reconnect, or plug the cable in again.")
                            : phone.state === "no permissions"
                              ? qsTr("This computer isn't allowed to use the phone's USB port. On Linux, add an udev rule for Android phones.")
                            : phone.state !== "device"
                              ? qsTr("Waiting for the phone (%1).").arg(phone.state)
                            : phone.app === "missing"
                              ? (usbCard.usb.hasApk ? qsTr("Installing NectarLink on the phone…")
                                                    : qsTr("Install the NectarLink app on the phone."))
                            : qsTr("Opening NectarLink on the phone and connecting…")
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
                    iconPath: action === "install" ? Icons.download : action === "reconnect" ? Icons.refresh : Icons.phone
                    text: action === "install" ? qsTr("Try again")
                        : action === "reconnect" ? qsTr("Reconnect")
                        : qsTr("Open on phone")
                    onClicked: AppController.usbSetupAction(phone.serial, action)
                }
            }
        }
    }
}
