package dev.sieve.vpn

import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.util.concurrent.ExecutorService

/** Sends a datagram to `dstIp:dstPort` outside the VPN and returns the reply, or null on timeout. */
fun interface DatagramRelay {
    fun exchange(dstIp: Int, dstPort: Int, payload: ByteArray): ByteArray?
}

class SocketDatagramRelay(private val protect: (DatagramSocket) -> Boolean) : DatagramRelay {
    override fun exchange(dstIp: Int, dstPort: Int, payload: ByteArray): ByteArray? {
        DatagramSocket().use { socket ->
            protect(socket)
            socket.soTimeout = TIMEOUT_MS
            val addr = InetAddress.getByAddress(
                byteArrayOf((dstIp ushr 24).toByte(), (dstIp ushr 16).toByte(), (dstIp ushr 8).toByte(), dstIp.toByte()),
            )
            socket.send(DatagramPacket(payload, payload.size, addr, dstPort))
            val buf = ByteArray(4096)
            val reply = DatagramPacket(buf, buf.size)
            return try {
                socket.receive(reply)
                buf.copyOf(reply.length)
            } catch (_: java.net.SocketTimeoutException) {
                null
            }
        }
    }

    private companion object {
        const val TIMEOUT_MS = 5000
    }
}

/**
 * Passes UDP through untouched, mainly so DNS keeps working. UDP is not
 * inspected. QUIC (UDP 443) is dropped so apps fall back to HTTPS over TCP,
 * which sieve can intercept.
 */
class UdpForwarder(
    private val relay: DatagramRelay,
    private val executor: ExecutorService,
    private val write: (ByteArray) -> Unit,
) {
    fun onDatagram(buf: ByteArray, ip: Packets.Ip, udp: Packets.Udp) {
        if (udp.dstPort == QUIC_PORT) return
        val payload = buf.copyOfRange(udp.dataOffset, udp.dataEnd)
        executor.execute {
            val reply = try {
                relay.exchange(ip.dst, udp.dstPort, payload)
            } catch (_: Exception) {
                null
            } ?: return@execute
            write(Packets.buildUdp(ip.dst, ip.src, udp.dstPort, udp.srcPort, reply, reply.size))
        }
    }

    private companion object {
        const val QUIC_PORT = 443
    }
}
