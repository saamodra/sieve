package dev.sieve.vpn

import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.net.Socket
import java.util.concurrent.ExecutorService
import java.util.concurrent.LinkedBlockingQueue

/** Opens the byte stream a captured connection is relayed over. */
fun interface Upstream {
    /**
     * Returns a stream already tunnelled to `dstIp:dstPort`, or throws. The real
     * implementation sends `CONNECT` to the sieve proxy; tests return a fake.
     */
    fun open(dstIp: Int, dstPort: Int): Conn
}

interface Conn {
    val input: InputStream
    val output: OutputStream
    fun shutdownOutput()
    fun close()
}

class SocketConn(private val socket: Socket, override val input: InputStream) : Conn {
    override val output: OutputStream = socket.getOutputStream()
    override fun shutdownOutput() {
        try {
            socket.shutdownOutput()
        } catch (_: IOException) {
        }
    }

    override fun close() {
        try {
            socket.close()
        } catch (_: IOException) {
        }
    }
}

/**
 * A small userspace TCP endpoint for connections captured from the tun device.
 *
 * Each SYN from an app is answered locally. The destination is then reached by
 * sending `CONNECT ip:port` to sieve, and the bytes are relayed both ways.
 * sieve sniffs TLS from the stream, so it sees the SNI and intercepts as usual.
 *
 * The tun device is a lossless in-kernel queue, so there is no retransmission
 * timer, no congestion control and no TCP options beyond MSS. Out-of-order
 * segments are dropped and re-ACKed.
 */
class TcpForwarder(
    private val upstream: Upstream,
    private val executor: ExecutorService,
    /** Writes one finished IP packet to the tun device. Must be thread-safe. */
    private val write: (ByteArray) -> Unit,
) {
    private val sessions = HashMap<Key, Session>()

    private data class Key(val srcIp: Int, val srcPort: Int, val dstIp: Int, val dstPort: Int)

    val activeSessions: Int get() = synchronized(sessions) { sessions.size }

    /** Handles one TCP segment from the app. Called from the single tun reader thread. */
    fun onSegment(buf: ByteArray, ip: Packets.Ip, tcp: Packets.Tcp) {
        val key = Key(ip.src, tcp.srcPort, ip.dst, tcp.dstPort)
        val session = synchronized(sessions) { sessions[key] }
        if (session == null) {
            when {
                tcp.has(Packets.RST) -> Unit
                tcp.has(Packets.SYN) && !tcp.has(Packets.ACK) -> start(key, tcp)
                else -> reset(key, tcp)
            }
            return
        }
        session.onSegment(buf, tcp)
    }

    fun closeAll() {
        val all = synchronized(sessions) { sessions.values.toList().also { sessions.clear() } }
        all.forEach { it.abort(sendRst = false) }
    }

    private fun start(key: Key, syn: Packets.Tcp) {
        val s = Session(key, syn.seq, syn.window)
        synchronized(sessions) { sessions[key] = s }
        executor.execute { s.connect() }
    }

    /** Answers a segment of an unknown connection with a RST, as a closed port would. */
    private fun reset(key: Key, tcp: Packets.Tcp) {
        val packet = if (tcp.has(Packets.ACK)) {
            Packets.buildTcp(key.dstIp, key.srcIp, key.dstPort, key.srcPort, tcp.ack, 0, Packets.RST, 0)
        } else {
            val ack = (tcp.seq + tcp.dataLength + (if (tcp.has(Packets.SYN)) 1 else 0) + (if (tcp.has(Packets.FIN)) 1 else 0)) and MASK
            Packets.buildTcp(key.dstIp, key.srcIp, key.dstPort, key.srcPort, 0, ack, Packets.RST or Packets.ACK, 0)
        }
        write(packet)
    }

    private inner class Session(private val key: Key, clientIsn: Long, window: Int) {
        @Suppress("PLATFORM_CLASS_MAPPED_TO_KOTLIN")
        private val lock = Object()

        // Sequence state, guarded by `lock`.
        private var rcvNext = (clientIsn + 1) and MASK
        private val iss = (System.nanoTime() and MASK)
        private var sndNext = (iss + 1) and MASK
        private var sndUna = sndNext
        private var peerWindow = window
        private var established = false
        private var clientFinned = false
        private var serverFinSent = false
        private var aborted = false

        private var conn: Conn? = null
        private val toServer = LinkedBlockingQueue<ByteArray>()

        fun connect() {
            val c = try {
                upstream.open(key.dstIp, key.dstPort)
            } catch (_: Exception) {
                abort(sendRst = true, ackSyn = true)
                return
            }
            synchronized(lock) {
                if (aborted) {
                    c.close()
                    return
                }
                conn = c
                established = true
                send(Packets.SYN or Packets.ACK, iss, rcvNext, EMPTY, 0)
            }
            executor.execute { pumpToServer(c) }
            executor.execute { pumpToClient(c) }
        }

        fun onSegment(buf: ByteArray, tcp: Packets.Tcp) {
            synchronized(lock) {
                if (aborted) return
                if (tcp.has(Packets.RST)) {
                    abortLocked(sendRst = false)
                    return
                }
                if (!established) return // SYN retransmit while the upstream connects
                if (tcp.has(Packets.SYN)) return // SYN retransmit after the SYN-ACK; the client will resend its ACK
                if (tcp.has(Packets.ACK)) {
                    if (seqLe(sndUna, tcp.ack) && seqLe(tcp.ack, sndNext)) sndUna = tcp.ack
                    peerWindow = tcp.window
                    lock.notifyAll()
                }
                var needAck = false
                val len = tcp.dataLength
                if (len > 0) {
                    if (tcp.seq == rcvNext && !clientFinned) {
                        toServer.add(buf.copyOfRange(tcp.dataOffset, tcp.dataEnd))
                        rcvNext = (rcvNext + len) and MASK
                    }
                    needAck = true
                }
                if (tcp.has(Packets.FIN) && !clientFinned && (tcp.seq + len) and MASK == rcvNext) {
                    clientFinned = true
                    rcvNext = (rcvNext + 1) and MASK
                    toServer.add(EOF)
                    needAck = true
                } else if (tcp.has(Packets.FIN)) {
                    needAck = true
                }
                if (needAck) send(Packets.ACK, sndNext, rcvNext, EMPTY, 0)
                maybeFinish()
            }
        }

        /** Copies bytes the app sent to the proxy connection. */
        private fun pumpToServer(c: Conn) {
            try {
                while (true) {
                    val chunk = toServer.take()
                    if (chunk === EOF) {
                        c.shutdownOutput()
                        return
                    }
                    c.output.write(chunk)
                    c.output.flush()
                }
            } catch (_: Exception) {
                abort(sendRst = true)
            }
        }

        /** Copies bytes from the proxy connection to the app, within the app's receive window. */
        private fun pumpToClient(c: Conn) {
            val buf = ByteArray(Packets.MSS)
            try {
                while (true) {
                    synchronized(lock) {
                        while (!aborted && ((sndNext - sndUna) and MASK) >= peerWindow.coerceAtLeast(1)) {
                            lock.wait(200)
                        }
                        if (aborted) return
                    }
                    val n = c.input.read(buf)
                    synchronized(lock) {
                        if (aborted) return
                        if (n < 0) {
                            serverFinSent = true
                            send(Packets.FIN or Packets.ACK, sndNext, rcvNext, EMPTY, 0)
                            sndNext = (sndNext + 1) and MASK
                            maybeFinish()
                            return
                        }
                        send(Packets.ACK or Packets.PSH, sndNext, rcvNext, buf, n)
                        sndNext = (sndNext + n) and MASK
                    }
                }
            } catch (_: Exception) {
                abort(sendRst = true)
            }
        }

        fun abort(sendRst: Boolean, ackSyn: Boolean = false) {
            synchronized(lock) { abortLocked(sendRst, ackSyn) }
        }

        private fun abortLocked(sendRst: Boolean, ackSyn: Boolean = false) {
            if (aborted) return
            aborted = true
            if (sendRst) {
                if (ackSyn) {
                    send(Packets.RST or Packets.ACK, 0, rcvNext, EMPTY, 0)
                } else {
                    send(Packets.RST or Packets.ACK, sndNext, rcvNext, EMPTY, 0)
                }
            }
            lock.notifyAll()
            toServer.clear()
            toServer.add(EOF)
            conn?.close()
            synchronized(sessions) { sessions.remove(key) }
        }

        /** Drops the session once both directions are closed and the server FIN is acknowledged. */
        private fun maybeFinish() {
            if (clientFinned && serverFinSent && sndUna == sndNext) {
                aborted = true
                lock.notifyAll()
                conn?.close()
                synchronized(sessions) { sessions.remove(key) }
            }
        }

        private fun send(flags: Int, seq: Long, ack: Long, data: ByteArray, len: Int) {
            write(
                Packets.buildTcp(
                    key.dstIp, key.srcIp, key.dstPort, key.srcPort,
                    seq, ack, flags, RECEIVE_WINDOW, data, len,
                ),
            )
        }
    }

    companion object {
        private const val MASK = 0xFFFFFFFFL
        private const val RECEIVE_WINDOW = 65535
        private val EMPTY = ByteArray(0)
        private val EOF = ByteArray(0)

        /** a <= b in 32-bit sequence space. */
        internal fun seqLe(a: Long, b: Long): Boolean = ((b - a) and MASK) < 0x80000000L
    }
}
