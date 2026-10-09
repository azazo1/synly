package com.azazo1.synly.core

import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothSocket
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelUuid
import java.util.Locale
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ScheduledThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference
import uniffi.synly_core.FfiBluetoothAvailability
import uniffi.synly_core.FfiBluetoothException
import uniffi.synly_core.FfiBluetoothPeer
import uniffi.synly_core.FfiBluetoothProvider

// 系统已配对设备的安全 RFCOMM 提供者, 不负责系统配对.
@Suppress("MissingPermission", "DEPRECATION")
class BluetoothBackend(context: Context) : FfiBluetoothProvider {
    private val application = context.applicationContext
    private val sockets = ConcurrentHashMap<ULong, NativeSocket>()
    private val nextSocket = AtomicLong(1)
    private val deadlines = ScheduledThreadPoolExecutor(1) { operation ->
        Thread(operation, "Synly Bluetooth deadline").apply { isDaemon = true }
    }.apply { removeOnCancelPolicy = true }

    companion object {
        private const val TAG = "Synly/Bluetooth"
        private const val MAX_FRAGMENT = 1024
        private const val MAX_SOCKETS = 16
        fun runtimePermissions(): Array<String> = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            arrayOf(Manifest.permission.BLUETOOTH_CONNECT, Manifest.permission.BLUETOOTH_SCAN)
        } else emptyArray()
    }

    override fun availability(): FfiBluetoothAvailability {
        return try {
            val adapter = application.getSystemService(BluetoothManager::class.java)?.adapter
                ?: return FfiBluetoothAvailability.UNSUPPORTED
            if (runtimePermissions().any { application.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }) {
                FfiBluetoothAvailability.PERMISSION_DENIED
            } else if (adapter.isEnabled) FfiBluetoothAvailability.AVAILABLE
            else FfiBluetoothAvailability.DISABLED
        } catch (_: SecurityException) {
            FfiBluetoothAvailability.PERMISSION_DENIED
        }
    }

    private fun adapter(): BluetoothAdapter {
        when (availability()) {
            FfiBluetoothAvailability.PERMISSION_DENIED -> throw FfiBluetoothException.PermissionDenied("请在系统中允许 Synly 使用附近设备")
            FfiBluetoothAvailability.DISABLED -> throw FfiBluetoothException.Disabled("系统蓝牙已关闭")
            FfiBluetoothAvailability.UNSUPPORTED -> throw FfiBluetoothException.Failed("当前系统没有可用的蓝牙控制器")
            FfiBluetoothAvailability.AVAILABLE -> Unit
        }
        return application.getSystemService(BluetoothManager::class.java)?.adapter
            ?: throw FfiBluetoothException.Failed("蓝牙控制器已移除")
    }

    private fun pairedDevice(address: String): BluetoothDevice {
        val canonical = address.uppercase(Locale.ROOT)
        if (!BluetoothAdapter.checkBluetoothAddress(canonical)) throw FfiBluetoothException.Failed("无效的蓝牙地址")
        val device = adapter().getRemoteDevice(canonical)
        if (device.bondState != BluetoothDevice.BOND_BONDED) throw FfiBluetoothException.Unpaired("设备尚未系统配对, 或配对已移除")
        return device
    }

    private inline fun <T> platform(operation: () -> T): T {
        try {
            return operation()
        } catch (error: FfiBluetoothException) {
            throw error
        } catch (error: SecurityException) {
            throw FfiBluetoothException.PermissionDenied(error.message ?: "系统拒绝蓝牙权限")
        } catch (error: Exception) {
            throw FfiBluetoothException.Failed(error.message ?: "系统蓝牙操作失败")
        }
    }

    override fun pairedDevices(): List<FfiBluetoothPeer> = platform {
        val devices = adapter().bondedDevices
        if (devices.size > 256) throw FfiBluetoothException.Failed("系统已配对设备数量超出枚举上限")
        devices.filter { it.bondState == BluetoothDevice.BOND_BONDED }
            .map { FfiBluetoothPeer(it.address.uppercase(Locale.ROOT), it.name ?: it.address) }
            .sortedBy { it.address }
    }

    private fun register(receiver: BroadcastReceiver, filter: IntentFilter) {
        // Bluetooth 广播可能来自非 system UID 的系统组件, 用 EXPORTED 接收受保护的系统广播.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            application.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        } else application.registerReceiver(receiver, filter)
    }

    override fun queryService(address: String, uuid: String): Boolean = platform {
        val device = pairedDevice(address)
        val expected = UUID.fromString(uuid)
        val response = CountDownLatch(1)
        val found = AtomicBoolean(false)
        val failure = AtomicReference<Exception?>()
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                if (intent?.action != BluetoothDevice.ACTION_UUID) return
                var target = false
                try {
                    val remote = intent.getParcelableExtra<BluetoothDevice>(BluetoothDevice.EXTRA_DEVICE) ?: return
                    if (!remote.address.equals(device.address, ignoreCase = true)) return
                    target = true
                    // 不信任广播中的 bond-state 数值, 重新向系统查询配对记录.
                    if (device.bondState == BluetoothDevice.BOND_BONDED) {
                        found.set(intent.getParcelableArrayExtra(BluetoothDevice.EXTRA_UUID)
                            ?.filterIsInstance<ParcelUuid>()?.any { it.uuid == expected } == true)
                    }
                } catch (error: Exception) {
                    failure.set(error)
                } finally {
                    if (target) response.countDown()
                }
            }
        }
        register(receiver, IntentFilter(BluetoothDevice.ACTION_UUID))
        val started = System.nanoTime()
        try {
            // 只查询已配对设备的服务, 不开始 inquiry 或扫描未配对设备.
            if (!device.fetchUuidsWithSdp()) return@platform false
            if (!response.await(10, TimeUnit.SECONDS)) throw FfiBluetoothException.Failed("蓝牙 SDP 查询超时")
            failure.get()?.let { throw it }
            found.get()
        } finally {
            runCatching { application.unregisterReceiver(receiver) }
            SynlyLog.i(TAG, "SDP 查询结束: $address, 耗时 ${(System.nanoTime() - started) / 1_000_000} ms")
        }
    }

    override fun createSocket(address: String, uuid: String): ULong = platform {
        val device = pairedDevice(address)
        synchronized(sockets) {
            if (sockets.size >= MAX_SOCKETS) throw FfiBluetoothException.Failed("蓝牙连接数量已达到上限")
            val id = nextSocket.getAndIncrement()
            if (id <= 0) throw FfiBluetoothException.Failed("蓝牙 socket 标识已耗尽")
            val native = NativeSocket(device, UUID.fromString(uuid))
            sockets[id.toULong()] = native
            id.toULong()
        }
    }

    private fun lookupSocket(handle: ULong): NativeSocket = sockets[handle]
        ?: throw FfiBluetoothException.Failed("蓝牙 socket 已关闭")

    override fun connectSocket(socket: ULong) = platform {
        adapter().cancelDiscovery()
        lookupSocket(socket).connect()
    }

    override fun readSocket(socket: ULong, maxBytes: UInt): ByteArray = platform {
        if (maxBytes == 0u || maxBytes > MAX_FRAGMENT.toUInt()) throw FfiBluetoothException.Failed("无效的蓝牙读取片段大小")
        lookupSocket(socket).read(maxBytes.toInt())
    }

    override fun writeSocket(socket: ULong, bytes: ByteArray) = platform {
        if (bytes.size > MAX_FRAGMENT) throw FfiBluetoothException.Failed("蓝牙写片段过大")
        lookupSocket(socket).write(bytes)
    }

    override fun closeSocket(socket: ULong) {
        sockets.remove(socket)?.close()
    }

    private inner class NativeSocket(private val device: BluetoothDevice, uuid: UUID) {
        // 只使用安全接口, 不反射固定通道或回退到 createInsecureRfcommSocketToServiceRecord.
        private val native: BluetoothSocket = device.createRfcommSocketToServiceRecord(uuid)
        private val closed = AtomicBoolean(false)
        private val registered = AtomicBoolean(false)
        private val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                try {
                    when (intent?.action) {
                        BluetoothDevice.ACTION_BOND_STATE_CHANGED -> {
                            val remote = intent.getParcelableExtra<BluetoothDevice>(BluetoothDevice.EXTRA_DEVICE)
                            if (remote?.address.equals(device.address, ignoreCase = true) && device.bondState != BluetoothDevice.BOND_BONDED) close()
                        }
                        BluetoothAdapter.ACTION_STATE_CHANGED -> if (!adapter().isEnabled) close()
                    }
                } catch (_: Exception) { close() }
            }
        }

        init {
            try {
                register(receiver, IntentFilter().apply {
                    addAction(BluetoothDevice.ACTION_BOND_STATE_CHANGED)
                    addAction(BluetoothAdapter.ACTION_STATE_CHANGED)
                })
                registered.set(true)
                if (closed.get()) unregister()
            } catch (error: Exception) {
                close()
                throw error
            }
        }

        private fun checkPaired() {
            if (closed.get()) throw FfiBluetoothException.Failed("蓝牙 socket 已关闭")
            if (device.bondState != BluetoothDevice.BOND_BONDED) {
                close()
                throw FfiBluetoothException.Unpaired("系统配对已移除")
            }
        }

        fun connect() {
            checkPaired()
            val started = System.nanoTime()
            val deadline = deadlines.schedule(Runnable { close() }, 15, TimeUnit.SECONDS)
            try {
                native.connect()
                checkPaired()
                if (!native.isConnected || native.connectionType != BluetoothSocket.TYPE_RFCOMM) {
                    throw FfiBluetoothException.Failed("系统未建立安全 RFCOMM 连接")
                }
                SynlyLog.i(TAG, "安全 RFCOMM 已连接: ${device.address}, 耗时 ${(System.nanoTime() - started) / 1_000_000} ms")
            } catch (error: Exception) {
                close()
                throw error
            } finally { deadline.cancel(false) }
        }

        fun read(maxBytes: Int): ByteArray {
            checkPaired()
            val bytes = ByteArray(maxBytes)
            val size = native.inputStream.read(bytes)
            return if (size < 0) ByteArray(0) else bytes.copyOf(size)
        }

        fun write(bytes: ByteArray) {
            checkPaired()
            val deadline = deadlines.schedule(Runnable { close() }, 10, TimeUnit.SECONDS)
            try { native.outputStream.write(bytes) }
            finally { deadline.cancel(false) }
        }

        private fun unregister() {
            if (registered.getAndSet(false)) runCatching { application.unregisterReceiver(receiver) }
        }

        fun close() {
            // 不能用同步锁包围 connect/read/write, 否则 close 无法打断阻塞 IO.
            if (closed.compareAndSet(false, true)) {
                runCatching { native.close() }.onFailure { SynlyLog.w(TAG, "关闭蓝牙 socket 失败", it) }
                unregister()
            }
        }
    }
}
