// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.elevated

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.AudioTrack
import java.io.FileDescriptor
import java.io.FileInputStream
import java.io.FileOutputStream
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import kotlin.system.exitProcess

/**
 * The sound of a phone call, for a PC: started by the PC over USB as the
 * shell user (`CLASSPATH=<apk> app_process / app.nectarlink.android.elevated.CallAudio`),
 * the way scrcpy starts its server, while a call is in progress.
 *
 * - stdout: what the other party says, 16 kHz mono 16-bit little-endian PCM,
 *   read with `AudioManager.getCallDownlinkExtractionAudioRecord` (Android 13+,
 *   `CALL_AUDIO_INTERCEPTION`, which the shell holds).
 * - stdin: what the PC's user says, in the same format, played into the
 *   call's uplink: an `AudioTrack` routed to the telephony TX device. (The
 *   platform's own `getCallUplinkInjectionAudioTrack` plays to the earpiece
 *   on some phones, the ZTE Blade A54 among them; the TX device works there.)
 *
 * It ends when the call does, when the PC closes stdin or stops reading, or
 * after [MAX_WAIT_MS] without a call. Nothing but sound goes to stdout; the
 * reason it ended goes to stderr.
 */
@SuppressLint("PrivateApi", "DiscouragedPrivateApi", "NewApi")
object CallAudio {
    private const val RATE = 16_000
    /** 20 ms of sound. */
    private const val CHUNK = RATE / 50 * 2
    private const val MAX_WAIT_MS = 10_000L
    /** The call counts as over once the phone has been out of call mode this long. */
    private const val ENDED_AFTER_MS = 1_500L

    @JvmStatic
    fun main(args: Array<String>) {
        val code = runCatching { run() }.getOrElse { System.err.println("call audio failed: $it"); 1 }
        System.err.flush()
        exitProcess(code)
    }

    private fun run(): Int {
        val audio = ShellContext.get().getSystemService(AudioManager::class.java)
        val deadline = System.currentTimeMillis() + MAX_WAIT_MS
        while (audio.mode != AudioManager.MODE_IN_CALL) {
            if (System.currentTimeMillis() > deadline) {
                System.err.println("no call in progress")
                return 2
            }
            Thread.sleep(200)
        }
        val interceptable = AudioManager::class.java.getMethod("isPstnCallAudioInterceptable").invoke(audio) as Boolean
        if (!interceptable) {
            System.err.println("this phone doesn't let call audio through")
            return 3
        }
        val inFormat = AudioFormat.Builder().setSampleRate(RATE).setChannelMask(AudioFormat.CHANNEL_IN_MONO)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT).build()
        val outFormat = AudioFormat.Builder().setSampleRate(RATE).setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT).build()
        val record = AudioManager::class.java.getMethod("getCallDownlinkExtractionAudioRecord", AudioFormat::class.java)
            .invoke(audio, inFormat) as AudioRecord
        val track = uplinkTrack(audio, outFormat)
        val out = FileOutputStream(FileDescriptor.out)
        val input = FileInputStream(FileDescriptor.`in`)

        val done = AtomicBoolean(false)
        val said = AtomicLong(0)
        record.startRecording()
        track?.play()
        // What the PC says: written as it arrives; the track's own buffer
        // smooths it, and a PC that sends nothing leaves the uplink alone.
        Thread {
            // stdin arrives in pieces of any size; the track takes whole
            // 16-bit samples only (it drops a stray byte, and every sample
            // after it would be off by one), so an odd byte waits for the next read.
            val buf = ByteArray(CHUNK + 1)
            var kept = 0
            try {
                while (!done.get()) {
                    val n = input.read(buf, kept, CHUNK)
                    if (n < 0) break
                    val have = kept + n
                    val whole = have and 1.inv()
                    if (whole > 0 && track != null) {
                        val wrote = track.write(buf, 0, whole)
                        if (said.getAndAdd(whole.toLong()) < RATE * 2 && said.get() >= RATE * 2) {
                            System.err.println("uplink: first second written ($wrote), routed to ${track.routedDevice?.type}")
                        }
                    }
                    kept = have - whole
                    if (kept == 1) buf[0] = buf[whole]
                }
            } catch (_: Exception) {
            }
            done.set(true)
        }.apply { isDaemon = true; start() }
        // Watches for the call to end.
        Thread {
            var outSince = 0L
            while (!done.get()) {
                if (audio.mode != AudioManager.MODE_IN_CALL) {
                    if (outSince == 0L) outSince = System.currentTimeMillis()
                    if (System.currentTimeMillis() - outSince > ENDED_AFTER_MS) done.set(true)
                } else {
                    outSince = 0L
                }
                Thread.sleep(200)
            }
        }.apply { isDaemon = true; start() }

        val buf = ByteArray(CHUNK)
        var reason = "the call ended"
        while (!done.get()) {
            val n = record.read(buf, 0, buf.size)
            if (n < 0) { reason = "recording stopped ($n)"; break }
            if (n == 0) { Thread.sleep(10); continue }
            try {
                out.write(buf, 0, n)
            } catch (_: Exception) {
                reason = "the PC stopped listening"
                break
            }
        }
        done.set(true)
        runCatching { record.stop() }
        record.release()
        track?.let { runCatching { it.stop() }; it.release() }
        System.err.println("$reason; uplink bytes=${said.get()}")
        return 0
    }

    /** A track playing into the call's uplink, or null if this phone has no telephony TX device. */
    private fun uplinkTrack(audio: AudioManager, format: AudioFormat): AudioTrack? {
        val tx = audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
            .firstOrNull { it.type == AudioDeviceInfo.TYPE_TELEPHONY }
            ?: return null.also { System.err.println("no telephony TX device; listening only") }
        val attributes = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build()
        val track = AudioTrack.Builder()
            .setAudioAttributes(attributes)
            .setAudioFormat(format)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .setBufferSizeInBytes(CHUNK * 8)
            .build()
        track.setPreferredDevice(tx)
        return track
    }
}
