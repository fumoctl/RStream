package dev.rstream.app

import android.app.Activity
import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Tracks active Android VPN transports via ConnectivityManager + NetworkCallback and pushes
 * updates into Rust (src/android.rs -> stremio_torrent::set_android_vpn_state).
 */
object RStreamVpn {
    private val registered = AtomicBoolean(false)
    private val vpnNetworks = LinkedHashSet<Network>()

    private data class VpnSnapshot(
        val active: Boolean,
        val iface: String?,
        val handle: Long,
        val addrsCsv: String?,
        val dnsCsv: String?,
    )

    @JvmStatic
    external fun nativeVpnChanged(
        active: Boolean,
        iface: String?,
        netHandle: Long,
        addrsCsv: String?,
        dnsCsv: String?,
    )

    @JvmStatic
    fun init(activity: Activity) {
        val cm = activity.applicationContext.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return
        syncInitial(cm)
        if (!registered.compareAndSet(false, true)) return
        runCatching {
            val req = NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                .build()
            cm.registerNetworkCallback(req, object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    synchronized(vpnNetworks) { vpnNetworks.add(network) }
                    publish(cm)
                }

                override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                    synchronized(vpnNetworks) { vpnNetworks.add(network) }
                    publish(cm)
                }

                override fun onLost(network: Network) {
                    synchronized(vpnNetworks) { vpnNetworks.remove(network) }
                    publish(cm)
                }
            })
        }
    }

    @Suppress("DEPRECATION")
    private fun syncInitial(cm: ConnectivityManager) {
        runCatching {
            synchronized(vpnNetworks) {
                vpnNetworks.clear()
                for (net in cm.allNetworks) {
                    val caps = cm.getNetworkCapabilities(net) ?: continue
                    if (caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                        vpnNetworks.add(net)
                    }
                }
            }
            publish(cm)
        }
    }

    private fun publish(cm: ConnectivityManager) {
        val snap = synchronized(vpnNetworks) {
            val net = vpnNetworks.lastOrNull()
            val lp = net?.let { runCatching { cm.getLinkProperties(it) }.getOrNull() }
            val name = lp?.interfaceName
            val addrs = lp?.linkAddresses
                ?.mapNotNull { it.address?.hostAddress?.substringBefore('%')?.trim() }
                ?.filter { it.isNotEmpty() }
                ?.joinToString(",")
            val dns = lp?.dnsServers
                ?.mapNotNull { it.hostAddress?.substringBefore('%')?.trim() }
                ?.filter { it.isNotEmpty() }
                ?.joinToString(",")
            VpnSnapshot(net != null, name, net?.networkHandle ?: 0L, addrs, dns)
        }
        runCatching { nativeVpnChanged(snap.active, snap.iface, snap.handle, snap.addrsCsv, snap.dnsCsv) }
    }
}
