package dev.sieve.vpn

import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.io.PipedInputStream
import java.io.PipedOutputStream
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PacketsTest {
    private val a = 0x0A000001
    private val b = 0x5DB8D822

    @Test
    fun builtTcpPacketHasValidChecksumsAndParsesBack() {
        val data = "hello".toByteArray()
        val p = Packets.buildTcp(a, b, 40000, 443, 1000, 2000, Packets.PSH or Packets.ACK, 65535, data)
        assertTrue(Packets.checksumsValid(p))
        val ip = Packets.parseIp(p, p.size)!!
        val tcp = Packets.parseTcp(p, ip)!!
        assertEquals(a, ip.src)
        assertEquals(b, ip.dst)
        assertEquals(40000, tcp.srcPort)
        assertEquals(1000L, tcp.seq)
        assertEquals(2000L, tcp.ack)
        assertArrayEquals(data, p.copyOfRange(tcp.dataOffset, tcp.dataEnd))
    }

    @Test
    fun synCarriesAnMssOptionAndStillChecksOut() {
        val p = Packets.buildTcp(a, b, 1, 2, 0xFFFFFFFFL, 0, Packets.SYN, 100)
        assertTrue(Packets.checksumsValid(p))
        val tcp = Packets.parseTcp(p, Packets.parseIp(p, p.size)!!)!!
        assertEquals(0xFFFFFFFFL, tcp.seq)
        assertEquals(0, tcp.dataLength)
        assertEquals(Packets.MSS, Packets.u16(p, 20 + 22))
    }

    @Test
    fun builtUdpPacketRoundTrips() {
        val p = Packets.buildUdp(a, b, 5353, 53, byteArrayOf(1, 2, 3), 3)
        assertTrue(Packets.checksumsValid(p))
        val ip = Packets.parseIp(p, p.size)!!
        val udp = Packets.parseUdp(p, ip)!!
        assertEquals(53, udp.dstPort)
        assertArrayEquals(byteArrayOf(1, 2, 3), p.copyOfRange(udp.dataOffset, udp.dataEnd))
    }

    @Test
    fun rejectsIpv6AndTruncatedPackets() {
        assertEquals(null, Packets.parseIp(ByteArray(40).also { it[0] = 0x60 }, 40))
        val p = Packets.buildTcp(a, b, 1, 2, 0, 0, Packets.ACK, 0)
        assertEquals(null, Packets.parseIp(p, p.size - 1))
    }
}

class TcpForwarderTest {
    private val client = 0x0A000001
    private val server = 0x5DB8D822
    private val pool = Executors.newCachedThreadPool()
    private val sent = LinkedBlockingQueue<ByteArray>()

    private class FakeConn : Conn {
        val received = ByteArrayOutputStream()
        val toClientSource = PipedOutputStream()
        override val input: InputStream = PipedInputStream(toClientSource, 8192)
        override val output: OutputStream = received
        @Volatile var outputShutdown = false
        override fun shutdownOutput() {
            outputShutdown = true
        }

        override fun close() {
            try {
                toClientSource.close()
            } catch (_: IOException) {
            }
        }
    }

    @After
    fun tearDown() {
        pool.shutdownNow()
    }

    private fun forwarder(upstream: Upstream) = TcpForwarder(upstream, pool) { sent.add(it) }

    private fun feed(f: TcpForwarder, flags: Int, seq: Long, ack: Long, data: ByteArray = ByteArray(0)) {
        val p = Packets.buildTcp(client, server, 40000, 443, seq, ack, flags, 65535, data)
        val ip = Packets.parseIp(p, p.size)!!
        f.onSegment(p, ip, Packets.parseTcp(p, ip)!!)
    }

    private fun next(): Packets.Tcp {
        val p = sent.poll(2, TimeUnit.SECONDS)
        assertNotNull("expected a packet to the client", p)
        assertTrue(Packets.checksumsValid(p!!))
        val ip = Packets.parseIp(p, p.size)!!
        return Packets.parseTcp(p, ip)!!.also { lastBuf = p }
    }

    private var lastBuf = ByteArray(0)

    @Test
    fun relaysBytesBothWaysAndClosesCleanly() {
        val conn = FakeConn()
        var opened: Pair<Int, Int>? = null
        val f = forwarder { ip, port ->
            opened = ip to port
            conn
        }

        feed(f, Packets.SYN, 1000, 0)
        val synAck = next()
        assertTrue(synAck.has(Packets.SYN) && synAck.has(Packets.ACK))
        assertEquals(1001L, synAck.ack)
        assertEquals(server to 443, opened!!.let { it.first to it.second })
        val serverSeq = (synAck.seq + 1) and 0xFFFFFFFFL

        // App sends data; it reaches the proxy connection and is ACKed.
        feed(f, Packets.ACK, 1001, serverSeq)
        feed(f, Packets.ACK or Packets.PSH, 1001, serverSeq, "ping".toByteArray())
        val ack = next()
        assertEquals(1005L, ack.ack)
        waitFor { conn.received.size() == 4 }
        assertEquals("ping", conn.received.toString())

        // Proxy sends data; it reaches the app as a segment.
        conn.toClientSource.write("pong".toByteArray())
        conn.toClientSource.flush()
        val data = next()
        assertEquals("pong", String(lastBuf.copyOfRange(data.dataOffset, data.dataEnd)))
        assertEquals(serverSeq, data.seq)

        // App closes; the proxy side sees a half-close and the session ends after the proxy closes too.
        feed(f, Packets.ACK, 1005, serverSeq + 4)
        feed(f, Packets.FIN or Packets.ACK, 1005, serverSeq + 4)
        assertTrue(next().has(Packets.ACK))
        waitFor { conn.outputShutdown }
        conn.toClientSource.close()
        val fin = next()
        assertTrue(fin.has(Packets.FIN))
        feed(f, Packets.ACK, 1006, serverSeq + 5)
        waitFor { f.activeSessions == 0 }
    }

    @Test
    fun failedUpstreamAnswersTheSynWithRst() {
        val f = forwarder { _, _ -> throw IOException("refused") }
        feed(f, Packets.SYN, 500, 0)
        val rst = next()
        assertTrue(rst.has(Packets.RST))
        assertEquals(501L, rst.ack)
        waitFor { f.activeSessions == 0 }
    }

    @Test
    fun unknownConnectionGetsRst() {
        val f = forwarder { _, _ -> error("must not open") }
        feed(f, Packets.ACK or Packets.PSH, 10, 77, "x".toByteArray())
        val rst = next()
        assertTrue(rst.has(Packets.RST))
        assertEquals(77L, rst.seq)
    }

    @Test
    fun clientRstTearsDownTheSession() {
        val conn = FakeConn()
        val f = forwarder { _, _ -> conn }
        feed(f, Packets.SYN, 1, 0)
        next()
        feed(f, Packets.RST, 2, 0)
        waitFor { f.activeSessions == 0 }
    }

    @Test
    fun readsTheProxyStatusLineWithoutConsumingTunnelData() {
        val stream = "HTTP/1.1 200 Connection Established\r\nX: y\r\n\r\nTLSDATA".byteInputStream()
        assertEquals("HTTP/1.1 200 Connection Established", ProxyUpstream.readStatusLine(stream))
        assertEquals("TLSDATA", String(stream.readBytes()))
    }

    private fun waitFor(cond: () -> Boolean) {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
        while (!cond()) {
            assertTrue("timed out", System.nanoTime() < deadline)
            Thread.sleep(10)
        }
    }
}
