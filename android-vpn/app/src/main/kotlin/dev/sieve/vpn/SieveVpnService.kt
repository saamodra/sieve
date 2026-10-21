package dev.sieve.vpn

import android.content.Intent
import android.net.VpnService
import android.os.ParcelFileDescriptor
import android.util.Log
import java.io.FileInputStream
import java.io.FileOutputStream
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/** Captures the device's IPv4 traffic on a tun interface and forwards TCP to the sieve proxy. */
class SieveVpnService : VpnService() {
    private var tun: ParcelFileDescriptor? = null
    private var reader: Thread? = null
    private var executor: ExecutorService? = null
    private var tcp: TcpForwarder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopVpn()
            stopSelf()
            return START_NOT_STICKY
        }
        val host = intent?.getStringExtra(EXTRA_HOST) ?: DEFAULT_HOST
        val port = intent?.getIntExtra(EXTRA_PORT, DEFAULT_PORT) ?: DEFAULT_PORT
        stopVpn()
        try {
            startVpn(host, port)
        } catch (e: Exception) {
            Log.e(TAG, "could not start the VPN", e)
            State.error = e.message ?: e.javaClass.simpleName
            stopVpn()
            stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun startVpn(host: String, port: Int) {
        val pfd = Builder()
            .setSession("sieve")
            .setMtu(MTU)
            .addAddress(TUN_ADDRESS, 32)
            .addRoute("0.0.0.0", 0)
            // IPv6 is routed into the tun and dropped, so apps fall back to IPv4
            // instead of bypassing sieve over a v6 path.
            .addAddress(TUN_ADDRESS_V6, 128)
            .addRoute("::", 0)
            .addDnsServer(DNS_SERVER)
            .addDisallowedApplication(packageName)
            .establish() ?: throw IllegalStateException("VPN permission was not granted")
        tun = pfd

        val pool = Executors.newCachedThreadPool()
        executor = pool
        val out = FileOutputStream(pfd.fileDescriptor)
        val write: (ByteArray) -> Unit = { packet ->
            synchronized(out) {
                try {
                    out.write(packet)
                } catch (e: Exception) {
                    Log.w(TAG, "tun write failed", e)
                }
            }
        }
        val forwarder = TcpForwarder(ProxyUpstream(host, port) { protect(it) }, pool, write)
        val udp = UdpForwarder(SocketDatagramRelay { protect(it) }, pool, write)
        tcp = forwarder

        State.running = true
        State.error = null
        reader = Thread({ readLoop(FileInputStream(pfd.fileDescriptor), forwarder, udp) }, "sieve-tun").also {
            it.start()
        }
        Log.i(TAG, "VPN up, forwarding TCP to $host:$port")
    }

    private fun readLoop(input: FileInputStream, tcp: TcpForwarder, udp: UdpForwarder) {
        val buf = ByteArray(MTU + 100)
        try {
            while (!Thread.currentThread().isInterrupted) {
                val n = input.read(buf)
                if (n < 0) break
                if (n == 0) continue
                val ip = Packets.parseIp(buf, n) ?: continue
                when (ip.protocol) {
                    Packets.PROTO_TCP -> Packets.parseTcp(buf, ip)?.let { tcp.onSegment(buf, ip, it) }
                    Packets.PROTO_UDP -> Packets.parseUdp(buf, ip)?.let { udp.onDatagram(buf, ip, it) }
                }
            }
        } catch (e: Exception) {
            if (State.running) Log.w(TAG, "tun read ended", e)
        }
    }

    private fun stopVpn() {
        State.running = false
        reader?.interrupt()
        reader = null
        tcp?.closeAll()
        tcp = null
        executor?.shutdownNow()
        executor = null
        try {
            tun?.close()
        } catch (_: Exception) {
        }
        tun = null
    }

    override fun onRevoke() {
        stopVpn()
        super.onRevoke()
    }

    override fun onDestroy() {
        stopVpn()
        super.onDestroy()
    }

    /** Status shared with the activity; both run in this app's process. */
    object State {
        @Volatile var running = false
        @Volatile var error: String? = null
    }

    companion object {
        const val ACTION_STOP = "dev.sieve.vpn.STOP"
        const val EXTRA_HOST = "proxy_host"
        const val EXTRA_PORT = "proxy_port"
        const val DEFAULT_HOST = "127.0.0.1"
        const val DEFAULT_PORT = 8730
        private const val TAG = "SieveVpn"
        private const val MTU = 1500
        private const val TUN_ADDRESS = "10.215.173.1"
        private const val TUN_ADDRESS_V6 = "fd00:5173::1"
        private const val DNS_SERVER = "8.8.8.8"
    }
}
