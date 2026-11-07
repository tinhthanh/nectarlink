// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.app.Application

/** Starts the core with the process: a box on the cable may want it any time. */
class PhoneApp : Application() {
    val core: PhoneCore by lazy { PhoneCore(this) }

    override fun onCreate() {
        super.onCreate()
        core.start()
    }
}
