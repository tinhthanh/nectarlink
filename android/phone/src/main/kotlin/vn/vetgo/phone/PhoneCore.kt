// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.content.Context
import android.os.Build
import android.provider.Settings
import android.util.Log
import app.nectarlink.android.calls.PhoneCalls
import app.nectarlink.android.contacts.PhoneContacts
import app.nectarlink.android.core.KeystoreKeyProtector
import app.nectarlink.android.sms.PhoneSms
import app.nectarlink.core.DeviceInfo
import app.nectarlink.core.DeviceKind
import app.nectarlink.core.Event
import app.nectarlink.core.EventListener
import app.nectarlink.core.Link
import app.nectarlink.core.NectarlinkNode
import app.nectarlink.core.NodeOptions
import app.nectarlink.core.Notification
import app.nectarlink.core.PowerLevel
import app.nectarlink.core.initLogging
import app.nectarlink.core.installIdFrom
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * NectarLink's core, with only what vglink uses: texts, calls, contacts and
 * the notifications of chat apps. One per process ([PhoneApp.core]).
 */
class PhoneCore(context: Context) : EventListener {
    private val context = context.applicationContext
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val startLock = Mutex()
    /** Work for the node, one at a time and in order (reports of changes). */
    private val ops = Channel<suspend (NectarlinkNode) -> Unit>(Channel.UNLIMITED)

    @Volatile var node: NectarlinkNode? = null
        private set

    data class Pc(val id: String, val name: String, val online: Boolean, val usb: Boolean)

    data class State(
        val status: Status = Status.STARTING,
        val error: String? = null,
        val pcs: List<Pc> = emptyList(),
        val notificationAccess: Boolean = false,
    )

    enum class Status { STARTING, READY, FAILED }

    private val _state = MutableStateFlow(State())
    val state: StateFlow<State> = _state.asStateFlow()

    internal val sms = PhoneSms(this.context) { op { it.smsChanged(null) } }
    internal val calls = PhoneCalls(
        this.context,
        onChange = { call -> op { it.callChanged(call) } },
        onLogChanged = { op { it.callLogChanged() } },
    )
    internal val contacts = PhoneContacts(this.context) { op { it.contactsChanged() } }

    private fun op(work: suspend (NectarlinkNode) -> Unit) {
        if (node != null) ops.trySend(work)
    }

    /** Starts the node once; safe to call again from anywhere. */
    fun start() {
        scope.launch {
            startLock.withLock {
                if (node != null) return@withLock
                initLogging("info,iroh=warn,swarm_discovery=error")
                val options = NodeOptions(
                    dataDir = context.noBackupFilesDir.resolve("core").absolutePath,
                    device = deviceInfo(),
                    appVersion = BuildConfig.VERSION_NAME,
                    power = PowerLevel.BASIC,
                    awayMode = false,
                    downloadsDir = context.cacheDir.resolve("received").absolutePath,
                    usbPort = 53319u.toUShort(),
                )
                val started = try {
                    NectarlinkNode.start(options, SlimPlatform(context, this@PhoneCore), KeystoreKeyProtector(), this@PhoneCore)
                } catch (e: Exception) {
                    Log.e(TAG, "core failed to start", e)
                    _state.update { it.copy(status = Status.FAILED, error = e.message ?: e.javaClass.simpleName) }
                    return@withLock
                }
                node = started
                scope.launch { for (work in ops) runCatching { work(started) }.onFailure { Log.i(TAG, "a report failed", it) } }
                _state.update { it.copy(status = Status.READY, error = null) }
                refreshAccess()
                refreshPcs()
            }
        }
    }

    /**
     * Follows what the box granted: starts watching texts, calls and contacts
     * it may read, and tells the PCs what this phone offers now.
     */
    fun refreshAccess() {
        if (PhoneCalls.canFollow(context) || PhoneCalls.canReadLog(context)) calls.start() else calls.stop()
        if (PhoneContacts.canRead(context)) contacts.start() else contacts.stop()
        if (PhoneSms.canRead(context)) sms.start() else sms.stop()
        val access = ChatListener.hasAccess(context)
        _state.update { it.copy(notificationAccess = access) }
        op { it.updatePower(PowerLevel.BASIC, capabilities(access)) }
    }

    /** What this phone offers PCs (docs/protocol/capabilities.md), by what it may do. */
    private fun capabilities(notificationAccess: Boolean): List<String> = buildList {
        if (notificationAccess) addAll(listOf("notify.mirror", "notify.reply"))
        if (PhoneCalls.canFollow(context)) add("call.state")
        if (PhoneCalls.canControl(context)) add("call.control")
        if (PhoneCalls.canControl(context) && app.nectarlink.android.calls.CallCompanion.allowed(context)) add("call.incall")
        if (PhoneCalls.canReadLog(context)) add("call.log")
        if (PhoneCalls.canDial(context)) add("call.dial")
        if (PhoneCalls.canListSims(context)) add("call.sims")
        if (PhoneContacts.canRead(context)) add("contacts.read")
        if (PhoneSms.canRead(context)) add("sms.read")
        if (PhoneSms.canSend(context)) add("sms.send")
    }

    // ---- Notifications (from ChatListener) ----

    fun notificationAccessChanged(granted: Boolean, showing: List<Notification>) {
        _state.update { it.copy(notificationAccess = granted) }
        op {
            it.updatePower(PowerLevel.BASIC, capabilities(granted))
            it.notificationsReset(showing)
        }
    }

    fun notificationPosted(notification: Notification) = op { it.notificationPosted(notification) }

    fun notificationRemoved(key: String) = op { it.notificationRemoved(key) }

    // ---- Events from the core ----

    override fun onEvent(event: Event) {
        when (event) {
            is Event.DeviceAdded, is Event.DeviceRemoved, is Event.LinkChanged, is Event.PeerInfoChanged,
            is Event.Paired -> refreshPcs()
            else -> Unit
        }
    }

    private fun refreshPcs() {
        val pcs = runCatching { node?.pairedDevices().orEmpty() }.getOrDefault(emptyList()).map { d ->
            val link = d.link
            Pc(
                id = d.id,
                name = d.info.name,
                online = link is Link.Online,
                usb = (link as? Link.Online)?.usb == true,
            )
        }
        _state.update { it.copy(pcs = pcs) }
        if (pcs.isNotEmpty()) LinkService.start(context)
    }

    private fun deviceInfo(): DeviceInfo {
        val name = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() } ?: Build.MODEL
        return DeviceInfo(
            name = name,
            kind = DeviceKind.PHONE,
            os = "android",
            osVersion = Build.VERSION.RELEASE,
            model = "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
            accent = null,
            installId = runCatching {
                Settings.Secure.getString(context.contentResolver, Settings.Secure.ANDROID_ID)?.let { installIdFrom(it) }
            }.getOrNull(),
        )
    }

    companion object {
        private const val TAG = "VetgoPhone"
    }
}
