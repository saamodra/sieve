package dev.sieve.vpn

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.VpnService
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView

/** One screen: where the sieve proxy is, and a Connect/Disconnect button. */
class MainActivity : Activity() {
    private lateinit var hostField: EditText
    private lateinit var portField: EditText
    private lateinit var status: TextView
    private lateinit var button: Button
    private val handler = Handler(Looper.getMainLooper())
    private val refresh = object : Runnable {
        override fun run() {
            render()
            handler.postDelayed(this, 500)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("sieve", Context.MODE_PRIVATE)
        val pad = (16 * resources.displayMetrics.density).toInt()

        hostField = EditText(this).apply {
            hint = "Proxy host"
            setText(prefs.getString(SieveVpnService.EXTRA_HOST, SieveVpnService.DEFAULT_HOST))
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            setSingleLine()
        }
        portField = EditText(this).apply {
            hint = "Proxy port"
            setText(prefs.getInt(SieveVpnService.EXTRA_PORT, SieveVpnService.DEFAULT_PORT).toString())
            inputType = InputType.TYPE_CLASS_NUMBER
            setSingleLine()
        }
        status = TextView(this).apply { textSize = 16f }
        button = Button(this).apply { setOnClickListener { toggle() } }

        setContentView(
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.TOP
                setPadding(pad, pad, pad, pad)
                addView(TextView(context).apply {
                    text = "Forwards this device's TCP traffic to the sieve proxy. " +
                        "With sieve running on your Mac and the phone on USB, keep 127.0.0.1 and run `sieve android vpn`."
                })
                addView(hostField)
                addView(portField)
                addView(button)
                addView(status)
            },
        )

        applyIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        applyIntent(intent)
    }

    /** `sieve android vpn` launches the app with the proxy address and `autostart`. */
    private fun applyIntent(intent: Intent?) {
        intent?.getStringExtra(SieveVpnService.EXTRA_HOST)?.let { hostField.setText(it) }
        if (intent?.hasExtra(SieveVpnService.EXTRA_PORT) == true) {
            portField.setText(intent.getIntExtra(SieveVpnService.EXTRA_PORT, SieveVpnService.DEFAULT_PORT).toString())
        }
        if (intent?.getBooleanExtra("autostart", false) == true && !SieveVpnService.State.running) connect()
    }

    override fun onResume() {
        super.onResume()
        handler.post(refresh)
    }

    override fun onPause() {
        handler.removeCallbacks(refresh)
        super.onPause()
    }

    private fun toggle() {
        if (SieveVpnService.State.running) {
            startService(Intent(this, SieveVpnService::class.java).setAction(SieveVpnService.ACTION_STOP))
        } else {
            connect()
        }
    }

    private fun connect() {
        val consent = VpnService.prepare(this)
        if (consent != null) {
            startActivityForResult(consent, REQUEST_VPN)
        } else {
            startVpn()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode == REQUEST_VPN && resultCode == RESULT_OK) startVpn()
    }

    private fun startVpn() {
        val host = hostField.text.toString().trim().ifEmpty { SieveVpnService.DEFAULT_HOST }
        val port = portField.text.toString().toIntOrNull()?.takeIf { it in 1..65535 } ?: SieveVpnService.DEFAULT_PORT
        getSharedPreferences("sieve", Context.MODE_PRIVATE).edit()
            .putString(SieveVpnService.EXTRA_HOST, host)
            .putInt(SieveVpnService.EXTRA_PORT, port)
            .apply()
        startService(
            Intent(this, SieveVpnService::class.java)
                .putExtra(SieveVpnService.EXTRA_HOST, host)
                .putExtra(SieveVpnService.EXTRA_PORT, port),
        )
    }

    private fun render() {
        val running = SieveVpnService.State.running
        button.text = if (running) "Disconnect" else "Connect"
        status.text = when {
            running -> "Connected. Traffic goes through sieve."
            SieveVpnService.State.error != null -> "Failed: ${SieveVpnService.State.error}"
            else -> "Not connected."
        }
    }

    private companion object {
        const val REQUEST_VPN = 1
    }
}
