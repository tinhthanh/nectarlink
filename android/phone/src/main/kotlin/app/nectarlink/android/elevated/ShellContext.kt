// SPDX-License-Identifier: GPL-3.0-or-later
// The shell-side context CallAudio needs, copied from :app's AppDisplay.kt
// (the rest of that file is about app windows, which this app hasn't).
package app.nectarlink.android.elevated

import android.annotation.SuppressLint
import android.content.Context
import android.content.ContextWrapper
import android.os.Build
import android.os.Looper

/**
 * A context for system services in the shell-side helper, which has no app
 * of its own: the system context, speaking as the shell package (scrcpy
 * does the same).
 */
@SuppressLint("PrivateApi", "DiscouragedPrivateApi", "SoonBlockedPrivateApi", "BlockedPrivateApi")
internal class ShellContext private constructor(base: Context) : ContextWrapper(base) {
    override fun getPackageName(): String = SHELL
    override fun getOpPackageName(): String = SHELL
    override fun getApplicationContext(): Context = this

    @Suppress("NewApi")
    override fun getAttributionSource(): android.content.AttributionSource =
        android.content.AttributionSource.Builder(android.os.Process.SHELL_UID).setPackageName(SHELL).build()

    companion object {
        private const val SHELL = "com.android.shell"
        @Volatile private var instance: ShellContext? = null

        @Synchronized
        fun get(): ShellContext = instance ?: ShellContext(systemContext()).also { instance = it }

        private fun systemContext(): Context {
            if (Looper.getMainLooper() == null) {
                @Suppress("DEPRECATION")
                Looper.prepareMainLooper()
            }
            val type = Class.forName("android.app.ActivityThread")
            val constructor = type.getDeclaredConstructor().apply { isAccessible = true }
            val thread = constructor.newInstance()
            type.getDeclaredField("sCurrentActivityThread").apply { isAccessible = true }.set(null, thread)
            // Hidden-API rules are for apps; this runs in the shell's own process.
            runCatching { type.getDeclaredField("mSystemThread").apply { isAccessible = true }.setBoolean(thread, true) }
            fillConfigurationController(type, thread)
            return type.getDeclaredMethod("getSystemContext").invoke(thread) as Context
        }

        /**
         * A bare ActivityThread has no configuration controller (Android 13+). Samsung's
         * display sandboxing asks the thread for its configuration while the system context is
         * made, which then fails. Gives it a controller holding the default configuration.
         */
        private fun fillConfigurationController(type: Class<*>, thread: Any) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
            runCatching {
                val controllerType = Class.forName("android.app.ConfigurationController")
                val internal = Class.forName("android.app.ActivityThreadInternal")
                val controller = controllerType.getDeclaredConstructor(internal)
                    .apply { isAccessible = true }
                    .newInstance(thread)
                runCatching {
                    controllerType.getDeclaredField("mConfiguration").apply { isAccessible = true }
                        .set(controller, android.content.res.Configuration().apply { setToDefaults() })
                }
                type.getDeclaredField("mConfigurationController").apply { isAccessible = true }.set(thread, controller)
            }.onFailure { System.err.println("no configuration controller: $it") }
        }
    }
}

