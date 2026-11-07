// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** After a restart or an update: back to the box without anyone opening the app. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        (context.applicationContext as PhoneApp).core.start()
        LinkService.start(context)
    }
}
