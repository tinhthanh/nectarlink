// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.app.ActivityOptions
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import app.nectarlink.core.CallCommand
import app.nectarlink.core.CallLogEntry
import app.nectarlink.core.CallSim
import app.nectarlink.core.Contact
import app.nectarlink.core.FileToSend
import app.nectarlink.core.MediaAction
import app.nectarlink.core.MirrorInputEvent
import app.nectarlink.core.MirrorOptions
import app.nectarlink.core.NotificationFailure
import app.nectarlink.core.PhoneApp
import app.nectarlink.core.PhoneToggleValue
import app.nectarlink.core.PhotoAlbum
import app.nectarlink.core.PhotoItem
import app.nectarlink.core.PhotoThumb
import app.nectarlink.core.Platform
import app.nectarlink.core.SmsAttachment
import app.nectarlink.core.SmsMessage
import app.nectarlink.core.SmsPartData
import app.nectarlink.core.SmsThread
import app.nectarlink.core.StorageEntry
import app.nectarlink.core.StorageFailure
import app.nectarlink.core.StorageReadFile
import app.nectarlink.core.StorageWriteDone
import app.nectarlink.core.TaskNotify
import app.nectarlink.core.WatchFileOffer
import app.nectarlink.core.WebcamOptions

/**
 * What the core asks of this phone. Texts, calls, contacts and notification
 * actions are carried out; everything else NectarLink can do is refused,
 * and the phone doesn't offer it in its capabilities to begin with.
 */
internal class SlimPlatform(private val context: Context, private val core: PhoneCore) : Platform {

    // ---- Texts ----
    override fun smsThreads(limit: UInt): List<SmsThread> = core.sms.threads(limit.toInt())
    override fun smsMessages(thread: String, before: Long?, limit: UInt): List<SmsMessage> =
        core.sms.messages(thread, before, limit.toInt())
    override fun smsSend(to: List<String>, body: String): Boolean = core.sms.send(to, body)
    override fun sendSmsWithAttachments(to: List<String>, body: String, attachments: List<SmsAttachment>): Boolean =
        core.sms.send(to, body, attachments)
    override fun smsPart(id: String): SmsPartData? = core.sms.part(id)

    // ---- Calls and contacts ----
    override fun callCommand(id: String, command: CallCommand): Boolean = core.calls.command(id, command)
    override fun callLog(before: Long?, limit: UInt): List<CallLogEntry> = core.calls.callLog(before, limit.toInt())
    override fun callDial(number: String): Boolean = core.calls.dial(number)
    override fun callSims(): List<CallSim> = core.calls.sims()
    override fun callDialOn(number: String, sim: String): Boolean = core.calls.dialOn(number, sim)
    override fun contacts(query: String?, offset: UInt, limit: UInt): List<Contact> =
        core.contacts.list(query, offset.toInt(), limit.toInt())

    // ---- Notifications (replies to chats) ----
    override fun dismissNotification(key: String) {
        val listener = ChatListener.instance ?: throw NotificationFailure.Unsupported()
        listener.dismiss(key)
    }

    override fun runNotificationAction(key: String, action: String, reply: String?) {
        val listener = ChatListener.instance ?: throw NotificationFailure.Unsupported()
        val sbn = listener.find(key) ?: throw NotificationFailure.NotFound()
        val target = action.toIntOrNull()?.let { sbn.notification.actions?.getOrNull(it) }
            ?: throw NotificationFailure.NotFound()
        val intent = target.actionIntent ?: throw NotificationFailure.NotFound()
        // A reply goes to the app the way the notification shade sends it.
        val fillIn = Intent()
        if (reply != null) {
            val inputs = target.remoteInputs.orEmpty().filter { it.allowFreeFormInput }
            if (inputs.isEmpty()) throw NotificationFailure.NotFound()
            val results = Bundle().apply { inputs.forEach { putCharSequence(it.resultKey, reply) } }
            RemoteInput.addResultsToIntent(inputs.toTypedArray(), fillIn, results)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                RemoteInput.setResultsSource(fillIn, RemoteInput.SOURCE_FREE_FORM_INPUT)
            }
        }
        try {
            intent.send(context, 0, fillIn, null, null, null, sendOptions())
        } catch (e: PendingIntent.CanceledException) {
            throw NotificationFailure.NotFound()
        }
    }

    private fun sendOptions(): Bundle? {
        val mode = when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA -> ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOW_ALWAYS
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE -> {
                @Suppress("DEPRECATION")
                ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED
            }
            else -> return null
        }
        return ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(mode).toBundle()
    }

    // ---- Not in this app ----
    override fun startRinging() = Unit
    override fun stopRinging() = Unit
    override fun setClipboard(text: String): Boolean = false
    override fun setClipboardRich(text: String, html: String?, rtf: String?): Boolean = false
    override fun setClipboardImage(mime: String, bytes: ByteArray): Boolean = false
    override fun mediaCommand(player: String, action: MediaAction, position: ULong?) = Unit
    override fun openLink(fromId: String, url: String): Boolean = false
    override fun watchOffered(fromId: String, player: String, title: String?, file: WatchFileOffer?): Boolean = false
    override fun taskNotify(fromId: String, task: TaskNotify): Boolean = false
    override fun openPhoto(id: String): FileToSend? = null
    override fun photoAlbums(): List<PhotoAlbum> = emptyList()
    override fun photoList(album: String?, before: Long?, beforeId: String, limit: UInt): List<PhotoItem> = emptyList()
    override fun photoThumbs(ids: List<String>): List<PhotoThumb> = emptyList()
    override fun mirrorRequested(pcId: String, options: MirrorOptions): Boolean = false
    override fun mirrorStopRequested(pcId: String, session: UInt) = Unit
    override fun mirrorKeyframeRequested(pcId: String, session: UInt) = Unit
    override fun mirrorResizeRequested(pcId: String, session: UInt, width: UInt, height: UInt) = Unit
    override fun mirrorPowerRequested(pcId: String, stayAwake: Boolean, screenOff: Boolean) = Unit
    override fun mirrorInput(pcId: String, session: UInt, input: MirrorInputEvent) = Unit
    override fun webcamRequested(pcId: String, options: WebcamOptions): Boolean = false
    override fun webcamStopRequested(pcId: String) = Unit
    override fun micRequested(pcId: String): Boolean = false
    override fun micStopRequested(pcId: String) = Unit
    override fun webcamKeyframeRequested(pcId: String) = Unit
    override fun cameraCaptureRequested(pcId: String, requestId: String, mode: String): Boolean = false
    override fun cameraCaptureCancelled(pcId: String, requestId: String) = Unit
    override fun phoneApps(): List<PhoneApp> = emptyList()
    override fun setPhoneToggle(id: String, value: PhoneToggleValue): Boolean = false
    override fun storageList(path: String): List<StorageEntry> = throw StorageFailure.Unsupported()
    override fun storageOpenRead(path: String): StorageReadFile = throw StorageFailure.Unsupported()
    override fun storageWrite(path: String, stagedPath: String, modified: Long?): StorageWriteDone =
        throw StorageFailure.Unsupported()
    override fun storageMkdir(path: String) = throw StorageFailure.Unsupported()
    override fun storageRename(from: String, to: String) = throw StorageFailure.Unsupported()
    override fun storageDelete(path: String, confirmed: Boolean) = throw StorageFailure.Unsupported()
}
