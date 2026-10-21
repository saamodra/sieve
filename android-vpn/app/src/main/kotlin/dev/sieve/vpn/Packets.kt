package dev.sieve.vpn

/** Parsing and building of the IPv4, TCP and UDP packets read from the tun device. */
object Packets {
    const val PROTO_TCP = 6
    const val PROTO_UDP = 17

    const val FIN = 0x01
    const val SYN = 0x02
    const val RST = 0x04
    const val PSH = 0x08
    const val ACK = 0x10

    /** Largest TCP payload per segment sent to the client. Fits an MTU of 1500 with headers. */
    const val MSS = 1400

    class Ip(
        val protocol: Int,
        val src: Int,
        val dst: Int,
        /** Offset of the transport header in the packet buffer. */
        val payloadOffset: Int,
        /** End of the IP packet (exclusive), from the total length field. */
        val end: Int,
    )

    class Tcp(
        val srcPort: Int,
        val dstPort: Int,
        val seq: Long,
        val ack: Long,
        val flags: Int,
        val window: Int,
        val dataOffset: Int,
        val dataEnd: Int,
    ) {
        val dataLength: Int get() = dataEnd - dataOffset
        fun has(flag: Int): Boolean = flags and flag != 0
    }

    class Udp(val srcPort: Int, val dstPort: Int, val dataOffset: Int, val dataEnd: Int)

    /** Returns the IPv4 header of `buf[0 until len]`, or null for anything else (IPv6, fragments, junk). */
    fun parseIp(buf: ByteArray, len: Int): Ip? {
        if (len < 20) return null
        val versionIhl = buf[0].toInt() and 0xFF
        if (versionIhl shr 4 != 4) return null
        val ihl = (versionIhl and 0x0F) * 4
        val total = u16(buf, 2)
        if (ihl < 20 || total < ihl || total > len) return null
        val fragment = u16(buf, 6)
        if (fragment and 0x3FFF != 0) return null // MF set or non-zero offset
        return Ip(buf[9].toInt() and 0xFF, i32(buf, 12), i32(buf, 16), ihl, total)
    }

    fun parseTcp(buf: ByteArray, ip: Ip): Tcp? {
        val o = ip.payloadOffset
        if (ip.end - o < 20) return null
        val dataOffset = o + (buf[o + 12].toInt() and 0xF0 shr 4) * 4
        if (dataOffset < o + 20 || dataOffset > ip.end) return null
        return Tcp(
            srcPort = u16(buf, o),
            dstPort = u16(buf, o + 2),
            seq = u32(buf, o + 4),
            ack = u32(buf, o + 8),
            flags = buf[o + 13].toInt() and 0x3F,
            window = u16(buf, o + 14),
            dataOffset = dataOffset,
            dataEnd = ip.end,
        )
    }

    fun parseUdp(buf: ByteArray, ip: Ip): Udp? {
        val o = ip.payloadOffset
        if (ip.end - o < 8) return null
        val length = u16(buf, o + 4)
        val end = minOf(ip.end, o + length)
        if (length < 8) return null
        return Udp(u16(buf, o), u16(buf, o + 2), o + 8, end)
    }

    /**
     * Builds an IPv4 + TCP packet. A SYN gets an MSS option and no window scale,
     * so the connection runs unscaled and the 16-bit window is exact.
     */
    fun buildTcp(
        src: Int,
        dst: Int,
        srcPort: Int,
        dstPort: Int,
        seq: Long,
        ack: Long,
        flags: Int,
        window: Int,
        data: ByteArray = EMPTY,
        dataLen: Int = data.size,
    ): ByteArray {
        val options = if (flags and SYN != 0) 4 else 0
        val tcpLen = 20 + options + dataLen
        val out = ByteArray(20 + tcpLen)
        ipHeader(out, out.size, PROTO_TCP, src, dst)
        val t = 20
        put16(out, t, srcPort)
        put16(out, t + 2, dstPort)
        put32(out, t + 4, seq)
        put32(out, t + 8, ack)
        out[t + 12] = (((20 + options) / 4) shl 4).toByte()
        out[t + 13] = flags.toByte()
        put16(out, t + 14, window)
        if (options != 0) {
            out[t + 20] = 2
            out[t + 21] = 4
            put16(out, t + 22, MSS)
        }
        System.arraycopy(data, 0, out, t + 20 + options, dataLen)
        put16(out, t + 16, transportChecksum(out, t, tcpLen, src, dst, PROTO_TCP))
        return out
    }

    fun buildUdp(src: Int, dst: Int, srcPort: Int, dstPort: Int, data: ByteArray, dataLen: Int): ByteArray {
        val udpLen = 8 + dataLen
        val out = ByteArray(20 + udpLen)
        ipHeader(out, out.size, PROTO_UDP, src, dst)
        put16(out, 20, srcPort)
        put16(out, 22, dstPort)
        put16(out, 24, udpLen)
        System.arraycopy(data, 0, out, 28, dataLen)
        val sum = transportChecksum(out, 20, udpLen, src, dst, PROTO_UDP)
        put16(out, 26, if (sum == 0) 0xFFFF else sum)
        return out
    }

    /** True when the checksums of a parsed IPv4 packet are valid. Used by tests. */
    fun checksumsValid(buf: ByteArray): Boolean {
        val ip = parseIp(buf, buf.size) ?: return false
        if (fold(sum(buf, 0, ip.payloadOffset, 0)) != 0xFFFF) return false
        val len = ip.end - ip.payloadOffset
        val pseudo = pseudoSum(ip.src, ip.dst, ip.protocol, len)
        return fold(sum(buf, ip.payloadOffset, len, pseudo)) == 0xFFFF
    }

    private fun ipHeader(out: ByteArray, total: Int, protocol: Int, src: Int, dst: Int) {
        out[0] = 0x45
        put16(out, 2, total)
        out[6] = 0x40 // don't fragment
        out[8] = 64
        out[9] = protocol.toByte()
        put32(out, 12, src.toLong() and 0xFFFFFFFFL)
        put32(out, 16, dst.toLong() and 0xFFFFFFFFL)
        put16(out, 10, fold(sum(out, 0, 20, 0)).inv() and 0xFFFF)
    }

    private fun transportChecksum(buf: ByteArray, off: Int, len: Int, src: Int, dst: Int, proto: Int): Int =
        fold(sum(buf, off, len, pseudoSum(src, dst, proto, len))).inv() and 0xFFFF

    private fun pseudoSum(src: Int, dst: Int, proto: Int, len: Int): Int {
        var s = (src ushr 16) + (src and 0xFFFF) + (dst ushr 16) + (dst and 0xFFFF) + proto + len
        s = (s and 0xFFFF) + (s ushr 16)
        return s
    }

    private fun sum(buf: ByteArray, off: Int, len: Int, init: Int): Int {
        var s = init.toLong() and 0xFFFFFFFFL
        var i = off
        val end = off + len
        while (i + 1 < end) {
            s += u16(buf, i)
            i += 2
        }
        if (i < end) s += (buf[i].toInt() and 0xFF) shl 8
        while (s ushr 16 != 0L) s = (s and 0xFFFF) + (s ushr 16)
        return s.toInt()
    }

    private fun fold(v: Int): Int {
        var s = v
        while (s ushr 16 != 0) s = (s and 0xFFFF) + (s ushr 16)
        return s
    }

    fun u16(b: ByteArray, o: Int): Int = (b[o].toInt() and 0xFF shl 8) or (b[o + 1].toInt() and 0xFF)
    fun u32(b: ByteArray, o: Int): Long = i32(b, o).toLong() and 0xFFFFFFFFL
    fun i32(b: ByteArray, o: Int): Int =
        (b[o].toInt() and 0xFF shl 24) or (b[o + 1].toInt() and 0xFF shl 16) or
            (b[o + 2].toInt() and 0xFF shl 8) or (b[o + 3].toInt() and 0xFF)

    fun put16(b: ByteArray, o: Int, v: Int) {
        b[o] = (v ushr 8).toByte()
        b[o + 1] = v.toByte()
    }

    fun put32(b: ByteArray, o: Int, v: Long) {
        b[o] = (v ushr 24).toByte()
        b[o + 1] = (v ushr 16).toByte()
        b[o + 2] = (v ushr 8).toByte()
        b[o + 3] = v.toByte()
    }

    /** Dotted-quad form of an address held in an Int. */
    fun ipString(a: Int): String =
        "${a ushr 24 and 0xFF}.${a ushr 16 and 0xFF}.${a ushr 8 and 0xFF}.${a and 0xFF}"

    private val EMPTY = ByteArray(0)
}
