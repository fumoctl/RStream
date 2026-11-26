package dev.rstream.app

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import java.lang.ref.WeakReference
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Keeps RStream's process and loopback OAuth callback listener (`127.0.0.1:53682`) unfrozen
 * while the user completes cloud sign-in in the browser (prevents Android 14+ Cached Apps
 * Freezer and OEM Hans/MILlet freezers from causing `ERR_CONNECTION_TIMED_OUT` on redirect).
 */
class RStreamOAuthService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        instance = this
        val nm = getSystemService(NotificationManager::class.java)
        if (nm != null && nm.getNotificationChannel(CHANNEL_ID) == null) {
            val ch = NotificationChannel(
                CHANNEL_ID,
                "Cloud Account Sign-In",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps local OAuth callback active while signing in"
                setShowBadge(false)
            }
            nm.createNotificationChannel(ch)
        }
        val launchIntent = packageManager.getLaunchIntentForPackage(packageName)?.apply {
            addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        }
        val pi = launchIntent?.let {
            PendingIntent.getActivity(
                this,
                0,
                it,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }
        val icon = applicationInfo.icon.takeIf { it != 0 } ?: android.R.drawable.stat_notify_sync
        val notif = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(icon)
            .setContentTitle("Waiting for cloud sign-in")
            .setContentText("Complete sign-in in your browser to connect remote")
            .setOngoing(true)
            .apply { if (pi != null) setContentIntent(pi) }
            .build()

        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_SHORT_SERVICE)
        } else {
            startForeground(NOTIF_ID, notif)
        }

        if (!wanted) {
            shutdown()
            return START_NOT_STICKY
        }

        if (wakeLock?.isHeld != true) {
            runCatching {
                val pm = getSystemService(PowerManager::class.java)
                wakeLock = pm?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "rstream:oauth")?.apply {
                    setReferenceCounted(false)
                    acquire(180_000L)
                }
            }
        }
        return START_NOT_STICKY
    }

    override fun onTimeout(startId: Int) {
        wanted = false
        shutdown()
    }

    override fun onDestroy() {
        releaseWakeLock()
        if (instance === this) instance = null
        super.onDestroy()
    }

    internal fun shutdown() {
        releaseWakeLock()
        runCatching { stopForeground(STOP_FOREGROUND_REMOVE) }
        stopSelf()
    }

    private fun releaseWakeLock() {
        runCatching {
            wakeLock?.let { if (it.isHeld) it.release() }
        }
        wakeLock = null
    }

    companion object {
        private const val CHANNEL_ID = "rstream_oauth"
        private const val NOTIF_ID = 53682

        @Volatile
        private var wanted = false

        @Volatile
        private var instance: RStreamOAuthService? = null

        fun start(context: Context) {
            wanted = true
            runCatching {
                context.startForegroundService(Intent(context, RStreamOAuthService::class.java))
            }.onFailure {
                Log.w("RStream", "Couldn't start RStreamOAuthService", it)
            }
        }

        fun stop() {
            wanted = false
            instance?.shutdown()
        }
    }
}

/**
 * Foreground `dataSync` service keeping BitTorrent / HTTP downloads, rclone uploads, and remote
 * SQLite / profile CRDT sync alive at full speed while RStream is backgrounded or closed.
 */
class RStreamTransferService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_PAUSE_ALL -> {
                Thread { runCatching { nativeTransferAction(1) } }.start()
                return START_NOT_STICKY
            }
            ACTION_CANCEL_ALL -> {
                Thread { runCatching { nativeTransferAction(2) } }.start()
                return START_NOT_STICKY
            }
        }
        instance = this
        ensureChannel(this)
        val notif = buildNotification(this)
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIF_ID, notif)
        }
        if (!wanted) {
            shutdown()
            return START_NOT_STICKY
        }
        acquireLocks()
        return START_STICKY
    }

    override fun onTimeout(startId: Int) {
        handleOsTimeout()
    }

    override fun onTimeout(startId: Int, fgsType: Int) {
        handleOsTimeout()
    }

    private fun handleOsTimeout() {
        wanted = false
        Thread { runCatching { nativeTransferAction(1) } }.start()
        shutdown()
    }

    override fun onDestroy() {
        releaseLocks()
        if (instance === this) instance = null
        super.onDestroy()
    }

    internal fun refreshNotification() {
        if (!wanted) {
            shutdown()
            return
        }
        val nm = getSystemService(NotificationManager::class.java) ?: return
        ensureChannel(this)
        runCatching { nm.notify(NOTIF_ID, buildNotification(this)) }
    }

    internal fun shutdown() {
        releaseLocks()
        runCatching { stopForeground(STOP_FOREGROUND_REMOVE) }
        stopSelf()
    }

    private fun acquireLocks() {
        if (wakeLock?.isHeld != true) {
            runCatching {
                val pm = getSystemService(PowerManager::class.java)
                wakeLock = pm?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "rstream:transfer")?.apply {
                    setReferenceCounted(false)
                    acquire(6L * 3600L * 1000L)
                }
            }
        }
        if (wifiLock?.isHeld != true) {
            runCatching {
                val wm = applicationContext.getSystemService(WifiManager::class.java)
                @Suppress("DEPRECATION")
                val mode = if (Build.VERSION.SDK_INT >= 29) {
                    WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                } else {
                    WifiManager.WIFI_MODE_FULL_HIGH_PERF
                }
                wifiLock = wm?.createWifiLock(mode, "rstream:wifi")?.apply {
                    setReferenceCounted(false)
                    acquire()
                }
            }
        }
    }

    private fun releaseLocks() {
        runCatching { wakeLock?.let { if (it.isHeld) it.release() } }
        wakeLock = null
        runCatching { wifiLock?.let { if (it.isHeld) it.release() } }
        wifiLock = null
    }

    companion object {
        private const val CHANNEL_ID = "rstream_transfers"
        private const val NOTIF_ID = 53683
        private const val ACTION_PAUSE_ALL = "dev.rstream.app.PAUSE_ALL"
        private const val ACTION_CANCEL_ALL = "dev.rstream.app.CANCEL_ALL"

        @Volatile private var appContext: Context? = null
        @Volatile private var currentActivity: Activity? = null
        @Volatile private var notifPermRequested = false
        @Volatile private var lifecycleRegistered = false
        @Volatile private var wanted = false
        @Volatile private var instance: RStreamTransferService? = null

        @Volatile private var stateRunning = 0
        @Volatile private var stateQueued = 0
        @Volatile private var stateDownloaded = 0L
        @Volatile private var stateTotal = 0L
        @Volatile private var stateTitle = ""
        @Volatile private var stateDbSync = false

        @JvmStatic external fun nativeTransferAction(code: Int)
        @JvmStatic external fun nativeAppLifecycle(resumed: Boolean)

        @JvmStatic
        fun init(activity: Activity) {
            appContext = activity.applicationContext
            currentActivity = activity
            if (!lifecycleRegistered) {
                lifecycleRegistered = true
                (activity.applicationContext as? Application)?.registerActivityLifecycleCallbacks(
                    object : Application.ActivityLifecycleCallbacks {
                        override fun onActivityCreated(a: Activity, b: Bundle?) { currentActivity = a }
                        override fun onActivityStarted(a: Activity) { currentActivity = a }
                        override fun onActivityResumed(a: Activity) {
                            currentActivity = a
                            Thread { runCatching { nativeAppLifecycle(true) } }.start()
                        }
                        override fun onActivityPaused(a: Activity) {
                            Thread { runCatching { nativeAppLifecycle(false) } }.start()
                        }
                        override fun onActivityStopped(a: Activity) {}
                        override fun onActivitySaveInstanceState(a: Activity, b: Bundle) {}
                        override fun onActivityDestroyed(a: Activity) {
                            if (currentActivity === a) currentActivity = null
                        }
                    }
                )
            }
        }

        @JvmStatic
        fun sync(
            running: Int,
            queued: Int,
            downloadedBytes: Long,
            totalBytes: Long,
            primaryTitle: String,
            dbSyncing: Boolean,
        ) {
            stateRunning = running
            stateQueued = queued
            stateDownloaded = downloadedBytes
            stateTotal = totalBytes
            stateTitle = primaryTitle
            stateDbSync = dbSyncing

            val active = running > 0 || queued > 0 || dbSyncing
            if (!active) {
                wanted = false
                instance?.shutdown()
                return
            }

            if (Build.VERSION.SDK_INT >= 33 && !notifPermRequested) {
                currentActivity?.let { act ->
                    if (act.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                        notifPermRequested = true
                        act.runOnUiThread {
                            runCatching {
                                act.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 53683)
                            }
                        }
                    }
                }
            }

            wanted = true
            val live = instance
            if (live != null) {
                live.refreshNotification()
                return
            }
            val ctx = appContext ?: currentActivity?.applicationContext ?: return
            runCatching {
                ctx.startForegroundService(Intent(ctx, RStreamTransferService::class.java))
            }.onFailure {
                Log.w("RStream", "Couldn't start RStreamTransferService", it)
            }
        }

        internal fun ensureChannel(ctx: Context) {
            val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
            if (nm.getNotificationChannel(CHANNEL_ID) == null) {
                val ch = NotificationChannel(
                    CHANNEL_ID,
                    "Downloads & Cloud Sync",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = "Background media downloads, rclone uploads, and library sync"
                    setShowBadge(false)
                }
                nm.createNotificationChannel(ch)
            }
        }

        internal fun buildNotification(ctx: Context): Notification {
            val totalJobs = stateRunning + stateQueued
            val pct = if (stateTotal > 0L) ((stateDownloaded * 100L) / stateTotal).coerceIn(0L, 100L).toInt() else -1
            val title = when {
                totalJobs > 0 && pct >= 0 -> "Saving $totalJobs item(s) · $pct%"
                totalJobs > 0 -> "Saving $totalJobs item(s)…"
                else -> "Syncing library & cloud database…"
            }
            val subtitle = when {
                stateTitle.isNotBlank() && stateDbSync -> "$stateTitle · syncing DB"
                stateTitle.isNotBlank() -> stateTitle
                stateDbSync -> "Updating remote index & watch progress"
                else -> "Running in background"
            }
            val launchIntent = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.apply {
                addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            }
            val contentPi = launchIntent?.let {
                PendingIntent.getActivity(ctx, 1, it, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
            }
            val icon = ctx.applicationInfo.icon.takeIf { it != 0 } ?: android.R.drawable.stat_sys_download
            val builder = Notification.Builder(ctx, CHANNEL_ID)
                .setSmallIcon(icon)
                .setContentTitle(title)
                .setContentText(subtitle)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
            if (contentPi != null) builder.setContentIntent(contentPi)
            if (totalJobs > 0) {
                if (pct >= 0) {
                    builder.setProgress(100, pct, false)
                } else {
                    builder.setProgress(100, 0, true)
                }
                val pausePi = PendingIntent.getService(
                    ctx,
                    2,
                    Intent(ctx, RStreamTransferService::class.java).setAction(ACTION_PAUSE_ALL),
                    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                )
                val cancelPi = PendingIntent.getService(
                    ctx,
                    3,
                    Intent(ctx, RStreamTransferService::class.java).setAction(ACTION_CANCEL_ALL),
                    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                )
                builder.addAction(Notification.Action.Builder(null, "Pause all", pausePi).build())
                builder.addAction(Notification.Action.Builder(null, "Cancel all", cancelPi).build())
            } else {
                builder.setProgress(100, 0, true)
            }
            return builder.build()
        }
    }
}

/**
 * Android Back closes what the Rust shell has open (a title page, Search, the videos,
 * the player) exactly like Esc does on desktop: it sends Esc to the page. Rust enables
 * it only while something is open, so Back on a top-level screen still leaves the app;
 * on a TV it is always on, so Back only goes back and the remote's Home key leaves.
 */
object RStreamBack {
    private var boundActivity: WeakReference<Activity>? = null
    private var callback: OnBackPressedCallback? = null
    @Volatile
    private var interceptBack = false

    @JvmStatic
    fun setEnabled(activity: Activity, enabled: Boolean) {
        interceptBack = enabled
        activity.runOnUiThread {
            val owner = activity as? ComponentActivity ?: return@runOnUiThread
            val cb = if (boundActivity?.get() === activity && callback != null) {
                callback!!
            } else {
                callback?.remove()
                val ref = WeakReference<Activity>(activity)
                boundActivity = ref
                object : OnBackPressedCallback(true) {
                    override fun handleOnBackPressed() {
                        val act = ref.get() ?: return
                        if (interceptBack) {
                            findWebView(act.window.decorView)?.evaluateJavascript(
                                "document.querySelector('.app')?.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',code:'Escape',bubbles:true}))",
                                null,
                            )
                        } else {
                            act.moveTaskToBack(true)
                        }
                    }
                }.also {
                    owner.onBackPressedDispatcher.addCallback(owner, it)
                    callback = it
                }
            }
            cb.isEnabled = true
        }
    }

    @JvmStatic
    fun openUrl(activity: Activity, url: String): String? {
        val isLocalOAuth = url.startsWith("http://127.0.0.1:") || url.startsWith("http://localhost:")
        val launch = {
            try {
                if (isLocalOAuth) {
                    // Start foreground shortService first while MainActivity is still top-resumed,
                    // then open an in-task Custom Tab so OEM freezers never suspend 127.0.0.1:53682.
                    RStreamOAuthService.start(activity)
                    val customTab = Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
                        addCategory(Intent.CATEGORY_BROWSABLE)
                        putExtras(Bundle().apply {
                            putBinder("android.support.customtabs.extra.SESSION", null)
                            putInt("android.support.customtabs.extra.SHARE_STATE", 2)
                            putBoolean("android.support.customtabs.extra.ENABLE_URLBAR_HIDING", false)
                        })
                    }
                    try {
                        activity.startActivity(customTab)
                    } catch (_: Throwable) {
                        val fallback = Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
                            addCategory(Intent.CATEGORY_BROWSABLE)
                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                        }
                        activity.startActivity(fallback)
                    }
                } else {
                    val intent = Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
                        addCategory(Intent.CATEGORY_BROWSABLE)
                        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    }
                    activity.startActivity(intent)
                }
                null
            } catch (t: Throwable) {
                if (isLocalOAuth) RStreamOAuthService.stop()
                Log.e("RStream", "openUrl failed for $url", t)
                t.message ?: t.javaClass.simpleName
            }
        }
        if (Looper.myLooper() == Looper.getMainLooper()) {
            return launch()
        }
        var error: String? = "Timed out launching Android browser"
        val latch = CountDownLatch(1)
        activity.runOnUiThread {
            error = launch()
            latch.countDown()
        }
        latch.await(5, TimeUnit.SECONDS)
        return error
    }

    @JvmStatic
    fun finishOAuth(activity: Activity, bringToFront: Boolean) {
        activity.runOnUiThread {
            RStreamOAuthService.stop()
            if (bringToFront) {
                runCatching {
                    activity.startActivity(
                        Intent(activity, activity.javaClass).apply {
                            addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
                        }
                    )
                }
            }
        }
    }
}

internal fun findWebView(view: View): WebView? {
    if (view is WebView) return view
    if (view is ViewGroup) for (i in 0 until view.childCount) findWebView(view.getChildAt(i))?.let { return it }
    return null
}
