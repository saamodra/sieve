package dev.sieve.vpn

import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.net.InetSocketAddress
import java.net.Socket

/** Reaches a destination by asking the sieve proxy for a CONNECT tunnel to it. */
class ProxyUpstream(
    private val host: String,
    private val port: Int,
    /** Exempts the socket from the VPN so it does not loop back into the tun. */
    private val protect: (Socket) -> Boolean,
) : Upstream {
    override fun open(dstIp: Int, dstPort: Int): Conn {
        val target = "${Packets.ipString(dstIp)}:$dstPort"
        val socket = Socket()
        try {
            protect(socket)
            socket.tcpNoDelay = true
            socket.connect(InetSocketAddress(host, port), CONNECT_TIMEOUT_MS)
            socket.soTimeout = CONNECT_TIMEOUT_MS
            socket.getOutputStream().apply {
                write("CONNECT $target HTTP/1.1\r\nHost: $target\r\n\r\n".toByteArray(Charsets.US_ASCII))
                flush()
            }
            val input = socket.getInputStream()
            val status = readStatusLine(input)
            if (!status.contains(" 200")) throw IOException("proxy refused CONNECT $target: $status")
            socket.soTimeout = 0
            return SocketConn(socket, input)
        } catch (e: Exception) {
            try {
                socket.close()
            } catch (_: IOException) {
            }
            throw e
        }
    }

    companion object {
        const val CONNECT_TIMEOUT_MS = 5000

        /**
         * Reads the proxy's response head up to the blank line and returns its
         * first line. Reads byte by byte so no tunnelled data is consumed.
         */
        fun readStatusLine(input: InputStream): String {
            val head = ByteArrayOutputStream()
            var tail = 0
            while (tail != END_OF_HEAD) {
                val b = input.read()
                if (b < 0) throw IOException("proxy closed the connection during CONNECT")
                head.write(b)
                tail = (tail shl 8) or b
                if (head.size() > MAX_HEAD) throw IOException("proxy response head too large")
            }
            return head.toString(Charsets.ISO_8859_1).substringBefore("\r\n")
        }

        private const val END_OF_HEAD = 0x0D0A0D0A
        private const val MAX_HEAD = 16 * 1024
    }
}
