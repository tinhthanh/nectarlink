// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.content.Context
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import androidx.core.app.NotificationManagerCompat
import app.nectarlink.android.notifications.LiveRateLimiter
import app.nectarlink.android.notifications.NotificationReader

/**
 * Hands this phone's notifications to the core, which turns chat apps' ones
 * (Messenger, Zalo, ...) into conversations on the box and can answer them.
 * The box allows it with `cmd notification allow_listener`; Android binds it
 * (and keeps the process alive) while that lasts. A slimmer copy of :app's
 * NotificationListener.
 */
class ChatListener : NotificationListenerService() {
    private val core get() = (application as PhoneApp).core
    private val reader by lazy { NotificationReader(this) }
    private val limiter = LiveRateLimiter { notification -> core.notificationPosted(notification) }
    private val worker = java.util.concurrent.Executors.newSingleThreadExecutor { r ->
        Thread(r, "vetgo-notif-reader").apply { isDaemon = true }
    }

    /** Keys already sent, so an update that shouldn't alert again doesn't. */
    private val sent = java.util.Collections.synchronizedSet(mutableSetOf<String>())

    override fun onListenerConnected() {
        instance = this
        core.start()
        work {
            val snapshot = runCatching { activeNotifications?.toList().orEmpty() }.getOrDefault(emptyList())
            val ranking = runCatching { currentRanking }.getOrNull()
            limiter.clear()
            sent.clear()
            val items = snapshot.mapNotNull { reader.read(it, ranking, update = false) }
            sent.addAll(items.map { it.key })
            core.notificationAccessChanged(true, items)
        }
    }

    override fun onListenerDisconnected() {
        if (instance === this) instance = null
        work {
            limiter.clear()
            sent.clear()
            core.notificationAccessChanged(false, emptyList())
        }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification, rankingMap: RankingMap?) {
        val key = sbn.key
        work {
            val notification = reader.read(sbn, rankingMap, update = key in sent)
            if (notification == null) {
                limiter.cancel(key)
                if (sent.remove(key)) core.notificationRemoved(key)
                return@work
            }
            sent.add(key)
            limiter.onPosted(notification)
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        val key = sbn.key
        work {
            limiter.cancel(key)
            if (sent.remove(key)) core.notificationRemoved(key)
        }
    }

    override fun onDestroy() {
        if (instance === this) instance = null
        super.onDestroy()
        worker.shutdown()
    }

    private fun work(task: () -> Unit) {
        try {
            worker.execute(task)
        } catch (_: java.util.concurrent.RejectedExecutionException) {
        }
    }

    internal fun dismiss(key: String) = cancelNotification(key)

    internal fun find(key: String): StatusBarNotification? =
        runCatching { getActiveNotifications(arrayOf(key)) }.getOrNull()?.firstOrNull()

    companion object {
        @Volatile
        internal var instance: ChatListener? = null
            private set

        fun hasAccess(context: Context): Boolean =
            NotificationManagerCompat.getEnabledListenerPackages(context).contains(context.packageName)
    }
}
