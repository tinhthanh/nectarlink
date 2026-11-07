// SPDX-License-Identifier: GPL-3.0-or-later
package vn.vetgo.phone

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import app.nectarlink.android.calls.PhoneCalls
import app.nectarlink.android.contacts.PhoneContacts
import app.nectarlink.android.sms.PhoneSms

/**
 * One screen: whether the box is connected and what it may use. Nothing to
 * set up here; the box does that over the cable.
 */
class MainActivity : ComponentActivity() {
    private val core get() = (application as PhoneApp).core

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        core.start()
        setContent {
            val dark = isSystemInDarkTheme()
            MaterialTheme(colorScheme = if (dark) darkColorScheme() else lightColorScheme()) {
                Surface(Modifier.fillMaxSize()) { Status(core) }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        // The box may have granted something since; offer it to the PCs.
        core.refreshAccess()
    }
}

@Composable
private fun Status(core: PhoneCore) {
    val state by core.state.collectAsStateWithLifecycle()
    val context = androidx.compose.ui.platform.LocalContext.current
    Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Spacer(Modifier.size(24.dp))
        Text("Vetgo Phone", fontSize = 26.sp, fontWeight = FontWeight.SemiBold)
        Text(
            "Tin nhắn và cuộc gọi của điện thoại này hiện trong Shipyard trên máy tính. Cắm cáp USB vào máy tính là đủ, máy tính tự cài đặt.",
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        val pc = state.pcs.firstOrNull { it.online } ?: state.pcs.firstOrNull()
        Line(
            ok = pc?.online == true,
            title = when {
                state.status == PhoneCore.Status.FAILED -> "Không khởi động được: ${state.error}"
                pc == null -> "Chưa ghép với máy tính nào"
                pc.online -> "Đã kết nối: ${pc.name}"
                else -> "Mất kết nối: ${pc.name}"
            },
            detail = when {
                pc == null -> "Cắm cáp USB vào máy tính có Shipyard và cho phép USB debugging."
                pc.usb -> "Qua cáp USB"
                pc.online -> "Qua mạng Wi‑Fi"
                else -> "Đang thử kết nối lại"
            },
        )
        Line(PhoneSms.canRead(context) && PhoneSms.canSend(context), "Tin nhắn SMS", null)
        Line(PhoneCalls.hasAll(context), "Cuộc gọi", null)
        Line(PhoneContacts.canRead(context), "Danh bạ", null)
        Line(state.notificationAccess, "Tin nhắn app (Messenger, Zalo…)", "qua thông báo")
    }
}

@Composable
private fun Line(ok: Boolean, title: String, detail: String?) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Spacer(
            Modifier.size(12.dp).background(if (ok) Color(0xFF2E9E5B) else Color(0xFFB0B0B0), CircleShape),
        )
        Spacer(Modifier.width(12.dp))
        Column {
            Text(title, fontSize = 16.sp)
            if (detail != null) Text(detail, fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}
